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
