//! Gemini uploads over local mocks.

use std::sync::Arc;

use super::*;
use leviath_testkit::{spawn_mock_recorder, spawn_mock_sequence, spawn_mock_server_with_headers};

fn upload() -> FileUpload {
    FileUpload {
        bytes: Arc::from(&b"....ftypmp42"[..]),
        mime_type: "video/mp4".into(),
        name: "clip.mp4".into(),
        ttl_secs: 3_600,
    }
}

fn provider(base: &str) -> GeminiProvider {
    GeminiProvider::new(reqwest::Client::new(), "g-key".into()).with_base_url(Some(base.into()))
}

const FAST: Duration = Duration::from_millis(1);

#[test]
fn the_upload_root_sits_after_the_host() {
    assert_eq!(
        upload_base("https://generativelanguage.googleapis.com/v1beta"),
        "https://generativelanguage.googleapis.com/upload/v1beta"
    );
    assert_eq!(
        upload_base("http://127.0.0.1:9"),
        "http://127.0.0.1:9/upload"
    );
    assert_eq!(upload_base("gateway"), "gateway/upload");
}

#[tokio::test]
async fn an_upload_starts_sends_the_bytes_and_waits_while_the_file_processes() {
    let (checks, checked) = spawn_mock_sequence(vec![
        (
            200,
            "OK",
            br#"{"name":"files/abc","state":"PROCESSING"}"#.to_vec(),
        ),
        (
            200,
            "OK",
            br#"{"name":"files/abc","state":"ACTIVE"}"#.to_vec(),
        ),
    ])
    .await;
    let processing = json!({ "file": { "name": "files/abc", "uri": format!("{checks}/v1beta/files/abc"),
        "state": "PROCESSING", "expirationTime": "1970-01-01T00:01:40Z" } });
    let (target, sent) =
        spawn_mock_sequence(vec![(200, "OK", processing.to_string().into_bytes())]).await;
    let (start, started) = spawn_mock_recorder_with_upload_url(&target).await;
    let file = provider(&format!("{start}/v1beta"))
        .upload(&upload(), FAST)
        .await
        .unwrap();
    assert_eq!(file.id, "files/abc");
    assert_eq!(file.uri, Some(format!("{checks}/v1beta/files/abc")));
    assert_eq!(file.expires_at, Some(100));
    let start_request = started.lock().unwrap().join("");
    assert!(
        start_request.contains("POST /upload/v1beta/files"),
        "{start_request}"
    );
    assert!(
        start_request
            .to_ascii_lowercase()
            .contains("x-goog-upload-command: start"),
        "{start_request}"
    );
    assert!(start_request.contains("clip.mp4"), "{start_request}");
    assert_eq!(
        sent.lock().unwrap()[0],
        "....ftypmp42",
        "the bytes went to the upload URL"
    );
    assert_eq!(checked.lock().unwrap().len(), 2, "checked until active");
}

/// A server that answers an upload start with `target` as the place to send
/// the bytes, recording what it was sent.
async fn spawn_mock_recorder_with_upload_url(
    target: &str,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let target = target.to_string();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = vec![0u8; 65536];
        let read = socket.read(&mut buf).await.unwrap_or(0);
        recorder
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(&buf[..read]).to_string());
        let response = format!(
            "HTTP/1.1 200 OK\r\nx-goog-upload-url: {target}/upload-here\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn an_upload_that_fails_says_where() {
    // The start answered without an upload URL.
    let no_url = spawn_mock_server_with_headers(200, "OK", "", b"{}".to_vec()).await;
    let err = provider(&no_url).upload(&upload(), FAST).await.unwrap_err();
    assert!(err.to_string().contains("no upload URL"), "{err}");

    // The start was refused.
    let (refused, _) = spawn_mock_recorder(403, "Forbidden", b"{}".to_vec()).await;
    assert!(provider(&refused).upload(&upload(), FAST).await.is_err());

    // Nothing is listening.
    assert!(
        provider("http://127.0.0.1:9")
            .upload(&upload(), FAST)
            .await
            .is_err()
    );

    // Google could not process it.
    let (target, _) = spawn_mock_sequence(vec![(
        200,
        "OK",
        br#"{"file":{"name":"files/x","state":"FAILED"}}"#.to_vec(),
    )])
    .await;
    let start = spawn_mock_server_with_headers(
        200,
        "OK",
        &format!("x-goog-upload-url: {target}/u\r\n"),
        b"{}".to_vec(),
    )
    .await;
    let err = provider(&start).upload(&upload(), FAST).await.unwrap_err();
    assert!(err.to_string().contains("could not process"), "{err}");

    // An answer with no name.
    let (target, _) =
        spawn_mock_sequence(vec![(200, "OK", br#"{"state":"ACTIVE"}"#.to_vec())]).await;
    let start = spawn_mock_server_with_headers(
        200,
        "OK",
        &format!("x-goog-upload-url: {target}/u\r\n"),
        b"{}".to_vec(),
    )
    .await;
    let err = provider(&start).upload(&upload(), FAST).await.unwrap_err();
    assert!(err.to_string().contains("no name"), "{err}");

    // The upload URL is unreachable.
    let start = spawn_mock_server_with_headers(
        200,
        "OK",
        "x-goog-upload-url: http://127.0.0.1:9/u\r\n",
        b"{}".to_vec(),
    )
    .await;
    assert!(provider(&start).upload(&upload(), FAST).await.is_err());

    // A processing check, with no uri to follow, that cannot connect.
    let (target, _) = spawn_mock_sequence(vec![(
        200,
        "OK",
        br#"{"file":{"name":"files/y","state":"PROCESSING"}}"#.to_vec(),
    )])
    .await;
    let (start, _) = spawn_mock_recorder_with_upload_url(&target).await;
    let checking = provider(&start);
    let err = checking.upload(&upload(), FAST).await.unwrap_err();
    assert!(err.to_string().contains("Request failed"), "{err}");
}

#[tokio::test(start_paused = true)]
async fn a_file_that_never_finishes_processing_is_given_up_on() {
    // One server for the bytes and every check, always answering that the
    // file is still processing, with its own address as the file's uri.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let served = format!("http://{}", listener.local_addr().unwrap());
    let body = json!({ "file": { "name": "files/z", "uri": format!("{served}/files/z"), "state": "PROCESSING" } })
        .to_string();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 65536];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    let (start, _) = spawn_mock_recorder_with_upload_url(&served).await;
    let err = provider(&start)
        .upload(&upload(), Duration::from_secs(120))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("still processing"), "{err}");
}

#[tokio::test]
async fn a_delete_of_a_gone_file_is_done() {
    let file = RemoteFile {
        id: "files/abc".into(),
        uri: None,
        expires_at: None,
    };
    for (status, reason, ok) in [
        (200, "OK", true),
        (404, "Not Found", true),
        (500, "Boom", false),
    ] {
        let (url, seen) = spawn_mock_recorder(status, reason, b"{}".to_vec()).await;
        assert_eq!(provider(&url).delete(&file).await.is_ok(), ok, "{status}");
        let raw = seen.lock().unwrap().join("");
        assert!(raw.contains("DELETE /files/abc"), "{raw}");
        assert!(raw.contains("x-goog-api-key: g-key"), "{raw}");
    }
    assert!(provider("http://127.0.0.1:9").delete(&file).await.is_err());
}

#[tokio::test]
async fn a_refused_finish_or_a_refused_check_is_an_error() {
    let (target, _) = spawn_mock_sequence(vec![(500, "Boom", b"{}".to_vec())]).await;
    let (start, _) = spawn_mock_recorder_with_upload_url(&target).await;
    assert!(provider(&start).upload(&upload(), FAST).await.is_err());

    let (checks, _) = spawn_mock_sequence(vec![(500, "Boom", b"{}".to_vec())]).await;
    let processing = json!({ "file": { "name": "files/c", "uri": format!("{checks}/files/c"), "state": "PROCESSING" } });
    let (target, _) =
        spawn_mock_sequence(vec![(200, "OK", processing.to_string().into_bytes())]).await;
    let (start, _) = spawn_mock_recorder_with_upload_url(&target).await;
    assert!(provider(&start).upload(&upload(), FAST).await.is_err());
}
