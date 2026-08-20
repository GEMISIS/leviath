//! Tests for the parent module. The standard child-module file name
//! (`tests.rs`) keeps this scaffolding outside the coverage report:
//! cargo-llvm-cov's default ignore regex excludes `tests.rs` and
//! `*_tests.rs` files, so only production code answers to the 100% gate.
use super::*;

fn ev(kind: &SseEvent) -> String {
    match kind {
        SseEvent::Data(s) => format!("data:{s}"),
        SseEvent::Done => "done".to_string(),
    }
}

#[test]
fn drain_splits_complete_events_only() {
    // A `event: ping` block (no data line) between the two data events is
    // dropped, exercising the None arm inside the drain loop.
    let mut buf =
        String::from("data: {\"a\":1}\n\nevent: ping\n\ndata: {\"b\":2}\n\ndata: partial");
    let events = drain_sse_events(&mut buf);
    assert_eq!(events.len(), 2);
    assert_eq!(ev(&events[0]), "data:{\"a\":1}");
    assert_eq!(ev(&events[1]), "data:{\"b\":2}");
    // The partial (no trailing blank line) stays buffered.
    assert_eq!(buf, "data: partial");
}

#[test]
fn done_sentinel_recognized() {
    assert!(matches!(
        parse_sse_block("data: [DONE]\n\n"),
        Some(SseEvent::Done)
    ));
}

#[test]
fn blocks_without_data_are_none() {
    assert!(parse_sse_block("event: ping\n\n").is_none());
    assert!(parse_sse_block("data:\n\n").is_none());
}

#[test]
fn final_event_flushes_untrimmed_tail() {
    assert!(matches!(
        final_sse_event("data: {\"x\":1}"),
        Some(SseEvent::Data(_))
    ));
    assert!(final_sse_event("   \n  ").is_none());
}

#[tokio::test]
async fn forward_sse_emits_payloads_then_stops_on_done() {
    let chunks: Vec<Result<bytes::Bytes, String>> = vec![
        Ok(bytes::Bytes::from("data: {\"a\":1}\n\n")),
        Ok(bytes::Bytes::from("data: {\"b\":2}\n\ndata: [DONE]\n\n")),
        Ok(bytes::Bytes::from("data: {\"never\":1}\n\n")),
    ];
    let stream = tokio_stream::iter(chunks);
    let (tx, mut rx) = mpsc::channel(16);
    forward_sse(stream, tx).await;
    let mut got = Vec::new();
    while let Some(item) = rx.recv().await {
        got.push(item.unwrap());
    }
    assert_eq!(got, vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]);
}

#[tokio::test]
async fn forward_sse_reports_read_error() {
    // A data event, then a read error: the error is forwarded and the stream
    // stops (the later event never arrives).
    let chunks: Vec<Result<bytes::Bytes, String>> = vec![
        Ok(bytes::Bytes::from("data: x\n\n")),
        Err("boom".to_string()),
        Ok(bytes::Bytes::from("data: y\n\n")),
    ];
    let (tx, mut rx) = mpsc::channel(16);
    forward_sse(tokio_stream::iter(chunks), tx).await;
    assert_eq!(rx.recv().await.unwrap().unwrap(), "x");
    assert!(matches!(
        rx.recv().await.unwrap(),
        Err(HostHttpError::Transport(msg)) if msg == "boom"
    ));
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn forward_sse_stops_when_receiver_dropped() {
    let chunks: Vec<Result<bytes::Bytes, String>> =
        vec![Ok(bytes::Bytes::from("data: a\n\ndata: b\n\n"))];
    let (tx, rx) = mpsc::channel(1);
    drop(rx); // receiver gone before we forward
    forward_sse(tokio_stream::iter(chunks), tx).await; // must return, not hang
}

#[tokio::test]
async fn forward_sse_flushes_trailing_event() {
    let chunks: Vec<Result<bytes::Bytes, String>> =
        vec![Ok(bytes::Bytes::from("data: {\"tail\":1}"))];
    let (tx, mut rx) = mpsc::channel(16);
    forward_sse(tokio_stream::iter(chunks), tx).await;
    assert_eq!(rx.recv().await.unwrap().unwrap(), "{\"tail\":1}");
}

#[tokio::test]
async fn forward_sse_skips_invalid_utf8_and_ends_clean() {
    // First chunk is invalid UTF-8 (skipped); then a complete event and a
    // clean end (no [DONE], empty buffer → no trailing flush).
    let chunks: Vec<Result<bytes::Bytes, String>> = vec![
        Ok(bytes::Bytes::from(vec![0xff, 0xfe])),
        Ok(bytes::Bytes::from("data: a\n\n")),
    ];
    let (tx, mut rx) = mpsc::channel(16);
    forward_sse(tokio_stream::iter(chunks), tx).await;
    assert_eq!(rx.recv().await.unwrap().unwrap(), "a");
    assert!(rx.recv().await.is_none());
}

#[test]
fn done_event_renders_in_helper() {
    // Covers the `SseEvent::Done` arm of the test `ev` helper.
    assert_eq!(ev(&SseEvent::Done), "done");
    assert_eq!(ev(&SseEvent::Data("x".to_string())), "data:x");
}

// ── ReqwestExecutor against a loopback mock server ───────────────────────

/// Spawn a one-shot HTTP/1.1 server on 127.0.0.1 that returns `status`,
/// `headers`, and `body`, then closes. Returns its `http://addr` URL.
async fn mock_server(status: &str, headers: &[(&str, &str)], body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf).await;
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.write_all(body.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
    });
    format!("http://{addr}")
}

fn req(method: HttpMethod, url: String, body: Option<&str>) -> HostRequest {
    let mut headers = BTreeMap::new();
    headers.insert("X-Test".to_string(), "1".to_string());
    HostRequest {
        method,
        url,
        body: body.map(str::to_string),
        headers,
        timeout_secs: Some(30),
    }
}

#[tokio::test]
async fn reqwest_executor_get_ok() {
    let url = mock_server("200 OK", &[], "hello-body").await;
    let out = ReqwestExecutor::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
    )
    .execute(req(HttpMethod::Get, url, None))
    .await
    .unwrap();
    assert_eq!(out, "hello-body");
}

#[tokio::test]
async fn reqwest_executor_post_with_body() {
    let url = mock_server("200 OK", &[], "{}").await;
    let out = ReqwestExecutor::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
    )
    .execute(req(HttpMethod::Post, url, Some("{\"a\":1}")))
    .await
    .unwrap();
    assert_eq!(out, "{}");
}

#[tokio::test]
async fn reqwest_executor_429_with_retry_after() {
    let url = mock_server("429 Too Many Requests", &[("Retry-After", "7")], "slow").await;
    let err = ReqwestExecutor::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
    )
    .execute(req(HttpMethod::Get, url, None))
    .await
    .err()
    .unwrap();
    assert!(matches!(
        err,
        HostHttpError::RateLimited {
            retry_after: Some(7)
        }
    ));
}

#[tokio::test]
async fn reqwest_executor_non_2xx_is_api_error() {
    let url = mock_server("500 Internal Server Error", &[], "boom").await;
    let err = ReqwestExecutor::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
    )
    .execute(req(HttpMethod::Get, url, None))
    .await
    .err()
    .unwrap();
    assert!(matches!(err, HostHttpError::Api(m) if m.contains("500")));
}

#[tokio::test]
async fn reqwest_executor_transport_error() {
    // Nothing listening on this port → connection refused.
    let err = ReqwestExecutor::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
    )
    .execute(req(
        HttpMethod::Get,
        "http://127.0.0.1:19998".to_string(),
        None,
    ))
    .await
    .err()
    .unwrap();
    assert!(matches!(err, HostHttpError::Transport(_)));
}

#[tokio::test]
async fn reqwest_executor_stream_forwards_events() {
    let url = mock_server(
        "200 OK",
        &[("Content-Type", "text/event-stream")],
        "data: {\"a\":1}\n\ndata: [DONE]\n\n",
    )
    .await;
    let (tx, mut rx) = mpsc::channel(16);
    ReqwestExecutor::new(crate::provider::build_http_client(None).expect("a test client builds"))
        .execute_stream(req(HttpMethod::Post, url, Some("{}")), tx)
        .await;
    assert_eq!(rx.recv().await.unwrap().unwrap(), "{\"a\":1}");
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn reqwest_executor_stream_transport_error() {
    let (tx, mut rx) = mpsc::channel(16);
    ReqwestExecutor::new(crate::provider::build_http_client(None).expect("a test client builds"))
        .execute_stream(
            req(
                HttpMethod::Post,
                "http://127.0.0.1:19998".to_string(),
                Some("{}"),
            ),
            tx,
        )
        .await;
    assert!(matches!(
        rx.recv().await.unwrap(),
        Err(HostHttpError::Transport(_))
    ));
}

#[tokio::test]
async fn reqwest_executor_body_read_error_is_transport() {
    // 200 promising more bytes than it delivers, then closes → the body read
    // fails, exercising execute()'s final text() error mapping.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf).await;
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 9999\r\nConnection: close\r\n\r\nshort";
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
    });
    let err = ReqwestExecutor::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
    )
    .execute(req(HttpMethod::Get, format!("http://{addr}"), None))
    .await
    .err()
    .unwrap();
    assert!(matches!(err, HostHttpError::Transport(_)));
}

#[tokio::test]
async fn reqwest_executor_non_2xx_body_read_error() {
    // A 500 that promises more body than it sends → classify()'s body read
    // fails and falls back to the error string (the unwrap_or_else arm).
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf).await;
        let head = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 9999\r\nConnection: close\r\n\r\nshort";
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
    });
    let err = ReqwestExecutor::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
    )
    .execute(req(HttpMethod::Get, format!("http://{addr}"), None))
    .await
    .err()
    .unwrap();
    assert!(matches!(err, HostHttpError::Api(_)));
}

#[tokio::test]
async fn reqwest_executor_stream_non_2xx_error() {
    let url = mock_server("503 Service Unavailable", &[], "down").await;
    let (tx, mut rx) = mpsc::channel(16);
    ReqwestExecutor::new(crate::provider::build_http_client(None).expect("a test client builds"))
        .execute_stream(req(HttpMethod::Post, url, Some("{}")), tx)
        .await;
    assert!(matches!(
        rx.recv().await.unwrap(),
        Err(HostHttpError::Api(_))
    ));
}

// ─── transport retry classification ─────────────────────────────────────────
//
// Driven as chain strings rather than `reqwest::Error`s: that type has no
// public constructor, and the detail these read sits several `source()` links
// down anyway. `error_chain` is what turns one into the other.

#[test]
fn h2_protocol_fault_is_recognised_and_retryable() {
    // Verbatim from the run archive of deep-researcher-1787212645, where this
    // permanently lost both investors.cerebras.ai primary sources.
    let chain = "error sending request for url (https://investors.cerebras.ai/x): \
                 client error (SendRequest): http2 error: stream error received: \
                 unexpected internal error encountered";
    assert!(is_h2_protocol_error(chain));
    assert!(is_retryable_transport(chain));
}

#[test]
fn transient_socket_failures_are_retryable() {
    for chain in [
        "error sending request: connection reset by peer",
        "connection closed before message completed",
        "os error 32: Broken pipe",
        "unexpected EOF during handshake",
        "operation timed out",
        "H2 error: GOAWAY",
    ] {
        assert!(is_retryable_transport(chain), "should retry: {chain}");
    }
}

#[test]
fn permanent_failures_are_not_retryable() {
    // Retrying these only spends the deadline before showing the agent the
    // same error, so they fall straight through.
    for chain in [
        "error sending request: dns error: failed to lookup address information",
        "tcp connect error: Connection refused (os error 61)",
        "invalid certificate: UnknownIssuer",
    ] {
        assert!(!is_retryable_transport(chain), "should not retry: {chain}");
        assert!(!is_h2_protocol_error(chain));
    }
}

#[test]
fn error_chain_walks_every_source() {
    #[derive(Debug)]
    struct Inner;
    impl std::fmt::Display for Inner {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("http2 error: stream error received")
        }
    }
    impl std::error::Error for Inner {}

    #[derive(Debug)]
    struct Outer(Inner);
    impl std::fmt::Display for Outer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("error sending request")
        }
    }
    impl std::error::Error for Outer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    let chain = error_chain(&Outer(Inner));
    assert_eq!(
        chain,
        "error sending request: http2 error: stream error received"
    );
    // The whole point: the classification only works on the flattened chain,
    // because the top-level message alone says nothing about h2.
    assert!(!is_h2_protocol_error("error sending request"));
    assert!(is_h2_protocol_error(&chain));
}

#[test]
fn backoff_grows_with_the_attempt() {
    assert_eq!(transport_backoff(1), std::time::Duration::from_millis(200));
    assert_eq!(transport_backoff(2), std::time::Duration::from_millis(400));
}

// ─── body readability ───────────────────────────────────────────────────────

#[test]
fn text_content_types_pass() {
    for ct in [
        Some("text/html; charset=utf-8"),
        Some("TEXT/PLAIN"),
        Some("application/json"),
        Some("application/xml"),
        Some("application/xhtml+xml"),
        Some("text/javascript"),
        Some("application/x-yaml"),
        Some("application/x-www-form-urlencoded"),
        // No header at all is not evidence of binary, so it passes here and
        // the mojibake check remains the backstop.
        None,
    ] {
        assert!(
            unsupported_content_type(ct).is_none(),
            "should pass: {ct:?}"
        );
    }
}

#[test]
fn binary_content_types_are_named() {
    let reason = unsupported_content_type(Some("application/pdf")).expect("named");
    assert!(reason.contains("application/pdf"), "{reason}");
    assert!(reason.contains("read bytes, not a document"), "{reason}");
    assert!(unsupported_content_type(Some("image/png")).is_some());
    assert!(unsupported_content_type(Some("application/octet-stream")).is_some());
}

#[test]
fn a_lossily_decoded_gzip_body_reads_as_mojibake() {
    // What `Response::text` produces from a gzip member: the few ASCII bytes of
    // the header survive and everything else becomes U+FFFD. finance.yahoo.com
    // serves exactly this when the client cannot decode `Content-Encoding`.
    let body: String = std::iter::repeat_n(char::REPLACEMENT_CHARACTER, 200).collect();
    assert!(looks_like_mojibake(&body));
    assert!(looks_like_mojibake(&format!("\u{1f}\u{8b}\u{8}{body}")));
}

#[test]
fn ordinary_prose_is_not_mojibake() {
    let prose = "Cerebras Systems introduced the CS-4, delivering 750 PFLOPS of AI \
                 compute across three WSE-3 Turbo processors, with first shipments \
                 beginning this quarter.";
    assert!(!looks_like_mojibake(prose));
    // A stray replacement character from one genuinely malformed byte is not
    // enough: the check is a ratio precisely so this stays readable.
    assert!(!looks_like_mojibake(&format!("{prose}\u{fffd}")));
}

#[test]
fn short_bodies_are_never_called_mojibake() {
    // Under the 64-char floor the ratio means nothing, and a short body has its
    // own diagnostic in the tool script.
    let tiny: String = std::iter::repeat_n(char::REPLACEMENT_CHARACTER, 8).collect();
    assert!(!looks_like_mojibake(&tiny));
    assert!(!looks_like_mojibake(""));
}

// ─── network-gated regression checks ────────────────────────────────────────
//
// `#[ignore]` so CI and the coverage gate never reach out. Run by hand with
// `cargo test -p leviath-providers -- --ignored --nocapture` when touching the
// transport. Both URLs are the ones that actually failed in
// deep-researcher-1787212645; keeping them named is the point.

#[tokio::test]
#[ignore = "hits the live network"]
async fn live_unsolicited_gzip_decodes_to_prose() {
    // finance.yahoo.com answers `Content-Encoding: gzip` even when nothing
    // asked for it. Without the reqwest gzip feature this returned a whole
    // gzip member that `text()` decoded into U+FFFD noise, and that noise went
    // straight into a research agent's `sources` region.
    let client = crate::provider::build_http_client(Some(30)).expect("client builds");
    let exec = ReqwestExecutor::new(client);
    let body = exec
        .execute(HostRequest {
            method: HttpMethod::Get,
            url: "https://finance.yahoo.com/quote/CBRS/".to_string(),
            body: None,
            headers: BTreeMap::new(),
            timeout_secs: Some(30),
        })
        .await
        .expect("fetch succeeds");
    assert!(!looks_like_mojibake(&body), "body decoded to mojibake");
    assert!(
        body.to_lowercase().contains("cerebras"),
        "expected readable prose, got {} chars starting {:?}",
        body.len(),
        body.chars().take(60).collect::<String>()
    );
}

// ─── retry loop and body guards, end to end through the executor ────────────

/// A server that kills the first `fail_first` connections without answering,
/// then serves `body` once. Reproduces "the socket did not work this time",
/// which is what the retry exists for.
async fn flaky_server(fail_first: usize, body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    tokio::spawn(async move {
        for i in 0.. {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            if i < fail_first {
                // Drop mid-request: the FIN reaches reqwest as
                // "connection closed before message completed", which is
                // exactly the transient shape the retry is for.
                drop(sock);
                continue;
            }
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn a_transient_transport_failure_is_retried() {
    // Before the retry existed this was a permanently lost source: one failed
    // send() and the URL was never read, while the bibliography still cited it.
    let url = flaky_server(1, "recovered-body").await;
    let out = ReqwestExecutor::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
    )
    .execute(req(HttpMethod::Get, url, None))
    .await
    .expect("the retry recovers");
    assert_eq!(out, "recovered-body");
}

#[tokio::test]
async fn retries_are_bounded() {
    // One more failure than the budget allows, so the error still surfaces
    // rather than the loop spinning.
    let url = flaky_server(usize::MAX, "never-served").await;
    let err = ReqwestExecutor::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
    )
    .execute(req(HttpMethod::Get, url, None))
    .await
    .expect_err("gives up");
    assert!(matches!(err, HostHttpError::Transport(_)), "{err:?}");
}

#[tokio::test]
async fn the_h1_fallback_client_serves_the_retry() {
    // The fallback path is only taken on an h2 fault, which a plaintext local
    // server cannot produce, so this proves the wiring: an executor holding a
    // fallback still succeeds normally, and `with_h1_fallback` is the
    // constructor the runtime uses.
    let url = flaky_server(1, "fallback-wired").await;
    let exec = ReqwestExecutor::with_h1_fallback(
        crate::provider::build_http_client(None).expect("a test client builds"),
        crate::provider::build_http1_client(None).expect("an h1 test client builds"),
    );
    let out = exec
        .execute(req(HttpMethod::Get, url, None))
        .await
        .expect("the retry recovers");
    assert_eq!(out, "fallback-wired");
}

#[tokio::test]
async fn a_binary_content_type_is_refused_before_the_body_is_read() {
    let url = mock_server("200 OK", &[("Content-Type", "application/pdf")], "%PDF-1.7").await;
    let err = ReqwestExecutor::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
    )
    .execute(req(HttpMethod::Get, url, None))
    .await
    .expect_err("refused");
    let HostHttpError::Api(msg) = err else {
        panic!("expected an Api error, got {err:?}");
    };
    assert!(msg.contains("unreadable body"), "{msg}");
    assert!(msg.contains("application/pdf"), "{msg}");
}

#[tokio::test]
async fn a_mojibake_body_is_refused_rather_than_returned() {
    // A body that survived the content-type check but decoded to noise. The
    // agent must be told the fetch failed, because it cannot tell this from a
    // page that genuinely says very little, and will cite it either way.
    const NOISE: &str = "\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\
                         \u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\
                         \u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\
                         \u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\
                         \u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\
                         \u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\
                         \u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\
                         \u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\
                         \u{fffd}\u{fffd}";
    let url = mock_server("200 OK", &[("Content-Type", "text/html")], NOISE).await;
    let err = ReqwestExecutor::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
    )
    .execute(req(HttpMethod::Get, url, None))
    .await
    .expect_err("refused");
    let HostHttpError::Api(msg) = err else {
        panic!("expected an Api error, got {err:?}");
    };
    assert!(msg.contains("did not read the page"), "{msg}");
}

#[test]
fn both_outbound_clients_build() {
    assert!(crate::provider::build_http_client(Some(5)).is_ok());
    assert!(crate::provider::build_http1_client(Some(5)).is_ok());
}

#[test]
fn the_h1_client_is_chosen_only_after_an_h2_fault() {
    let main = crate::provider::build_http_client(Some(5)).expect("main client builds");
    let h1 = crate::provider::build_http1_client(Some(5)).expect("h1 client builds");

    // The first attempt, and every attempt on an origin that never faulted,
    // stays on HTTP/2 so the ordinary case keeps its multiplexing.
    assert!(std::ptr::eq(retry_client(&main, Some(&h1), false), &main));
    // Once an h2 fault is seen, the retry goes over HTTP/1.1.
    assert!(std::ptr::eq(retry_client(&main, Some(&h1), true), &h1));
    // With no fallback configured there is nothing to switch to, so the flag
    // changes nothing rather than panicking.
    assert!(std::ptr::eq(retry_client(&main, None, true), &main));
    assert!(std::ptr::eq(retry_client(&main, None, false), &main));
}

#[test]
fn the_executor_survives_losing_only_its_fallback() {
    let ok = || crate::provider::build_http_client(Some(5)).expect("client builds");

    // Both clients: the ordinary case.
    assert!(executor_from_clients(Ok(ok()), Ok(ok())).is_ok());
    // No HTTP/1.1 twin is a degraded executor, not a dead one - every origin
    // that works over HTTP/2 still works, which is nearly all of them.
    assert!(executor_from_clients(Ok(ok()), Err(crate::provider::malformed_url_error())).is_ok());
    // No HTTPS client at all is the real failure, and it stays deferred to the
    // moment a script provider is resolved.
    assert!(executor_from_clients(Err(crate::provider::malformed_url_error()), Ok(ok()),).is_err());
}
