//! The shared file-storage pieces, over local mocks.

use super::*;
use crate::responses::client::{Auth, Endpoint};
use leviath_testkit::spawn_mock_recorder;

fn mime(s: &str) -> MimeType {
    MimeType::parse(s).unwrap()
}

fn upload(mime_type: &str) -> FileUpload {
    FileUpload {
        bytes: Arc::from(&b"%PDF-1.7 body"[..]),
        mime_type: mime_type.into(),
        name: "doc.pdf".into(),
        ttl_secs: 7 * 86_400,
    }
}

#[test]
fn each_vendor_names_its_limits_and_an_unknown_one_names_none() {
    let anthropic = provider_limits("anthropic");
    assert_eq!(
        anthropic.inline_part_limit(&mime("image/png")),
        Some(5 * MIB)
    );
    assert_eq!(anthropic.inline_part_limit(&mime("application/pdf")), None);
    assert!(anthropic.by_file(&mime("application/pdf"), 10 * MIB));
    assert!(!anthropic.by_file(&mime("video/mp4"), 1));
    assert!(!anthropic.by_file(&mime("image/png"), 501 * MIB));
    assert_eq!(anthropic.largest_part(), Some(500 * MIB));

    assert_eq!(provider_limits("grok"), provider_limits("xai"));
    assert!(provider_limits("meta").by_file(&mime("video/mp4"), MIB));
    assert!(!provider_limits("xai").by_file(&mime("image/png"), MIB));
    assert_eq!(
        provider_limits("bedrock").inline_part_limit(&mime("application/pdf")),
        Some(4_500_000)
    );
    assert_eq!(
        provider_limits("google").largest_part(),
        Some(2 * 1024 * MIB)
    );
    assert_eq!(provider_limits("openai").file_types.len(), 2);
    assert_eq!(provider_limits("openrouter"), MediaLimits::NONE);
    assert_eq!(MediaLimits::NONE.largest_part(), None);
    assert_eq!(
        provider_limits("bedrock").largest_part(),
        Some(25 * MIB),
        "an inline-only vendor's largest part is its largest inline limit"
    );
}

#[test]
fn inline_only_keeps_the_inline_limits_and_drops_the_files() {
    let meta = provider_limits("meta").inline_only();
    assert_eq!(meta.file_bytes, None);
    assert!(!meta.by_file(&mime("image/png"), 1));
    assert_eq!(meta.inline_request_bytes, Some(50 * MIB));
}

#[test]
fn a_lifetime_is_clamped_to_what_the_vendor_takes() {
    let xai = provider_limits("xai");
    assert_eq!(xai.clamp_ttl(10), MIN_TTL_SECS);
    assert_eq!(xai.clamp_ttl(86_400), 86_400);
    assert_eq!(xai.clamp_ttl(365 * 86_400), 30 * 86_400);
    assert_eq!(MediaLimits::NONE.clamp_ttl(99_999), MIN_TTL_SECS);
}

#[test]
fn a_file_is_usable_until_its_margin_before_expiry() {
    let mut file = RemoteFile {
        id: "f".into(),
        uri: None,
        expires_at: None,
    };
    assert!(file.usable(1_000, 600), "no expiry lives until deleted");
    file.expires_at = Some(2_000);
    assert!(file.usable(1_000, 600));
    assert!(!file.usable(1_500, 600));
}

#[test]
fn an_upload_answer_gives_the_id_and_an_expiry_in_either_spelling() {
    let numeric = remote_from(&serde_json::json!({ "id": "a", "expires_at": 42 }), 10).unwrap();
    assert_eq!(numeric.expires_at, Some(42));
    let text = remote_from(
        &serde_json::json!({ "id": "b", "expires_at": "1970-01-01T00:01:40Z" }),
        10,
    )
    .unwrap();
    assert_eq!(text.expires_at, Some(100));
    let before = now_secs();
    let none = remote_from(&serde_json::json!({ "id": "c", "expires_at": null }), 3_600).unwrap();
    assert!(none.expires_at.unwrap() >= before + 3_600);
    let garbled = remote_from(&serde_json::json!({ "id": "d", "expires_at": "soon" }), 60).unwrap();
    assert!(garbled.expires_at.is_some());
    assert!(remote_from(&serde_json::json!({ "id": "" }), 60).is_err());
    assert!(remote_from(&serde_json::json!({}), 60).is_err());
}

#[test]
fn a_missing_file_error_is_told_apart_from_other_failures() {
    assert!(names_a_missing_file(&ProviderError::ApiError(
        "HTTP 404: File `file_1` not found.".into()
    )));
    assert!(names_a_missing_file(&ProviderError::ApiError(
        "{\"error\":{\"type\":\"not_found_error\",\"message\":\"no such file\"}}".into()
    )));
    assert!(!names_a_missing_file(&ProviderError::ApiError(
        "HTTP 404: model not found".into()
    )));
    assert!(!names_a_missing_file(&ProviderError::ApiError(
        "file too large".into()
    )));
}

#[test]
fn an_upload_prints_its_size_not_its_bytes_and_a_bad_type_is_refused() {
    let debug = format!("{:?}", upload("application/pdf"));
    assert!(debug.contains("bytes: 13"), "{debug}");
    assert!(file_part(&upload("not a mime\n")).is_err());
}

#[tokio::test]
async fn an_openai_shaped_upload_sends_the_purpose_the_lifetime_and_the_file() {
    let (url, seen) = spawn_mock_recorder(
        200,
        "OK",
        br#"{"id":"file-9","expires_at":1790000000}"#.to_vec(),
    )
    .await;
    let endpoint = Endpoint::new(reqwest::Client::new(), &url, Auth::Key("xai-k".into()));
    let file = upload_openai_shape(
        &endpoint,
        &upload("application/pdf"),
        "assistants",
        &provider_limits("xai"),
    )
    .await
    .unwrap();
    assert_eq!(file.id, "file-9");
    assert_eq!(file.expires_at, Some(1_790_000_000));
    let raw = seen.lock().unwrap().join("");
    assert!(raw.contains("POST /files"), "{raw}");
    assert!(
        raw.contains("authorization: Bearer xai-k") || raw.contains("Authorization: Bearer xai-k"),
        "{raw}"
    );
    assert!(raw.contains("name=\"purpose\"\r\n\r\nassistants"), "{raw}");
    assert!(
        raw.contains("name=\"expires_after[anchor]\"\r\n\r\ncreated_at"),
        "{raw}"
    );
    assert!(
        raw.contains("name=\"expires_after[seconds]\"\r\n\r\n604800"),
        "{raw}"
    );
    assert!(raw.contains("Content-Type: application/pdf"), "{raw}");
    assert!(raw.contains("%PDF-1.7 body"), "{raw}");

    let (refused, _) = spawn_mock_recorder(413, "Too Large", b"{}".to_vec()).await;
    let endpoint = Endpoint::new(reqwest::Client::new(), &refused, Auth::Key("k".into()));
    let err = upload_openai_shape(
        &endpoint,
        &upload("application/pdf"),
        "x",
        &provider_limits("xai"),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("413"), "{err}");
    assert!(
        upload_openai_shape(&endpoint, &upload("bad\n"), "x", &provider_limits("xai"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_openai_shaped_delete_treats_a_gone_file_as_deleted() {
    let file = RemoteFile {
        id: "file-9".into(),
        uri: None,
        expires_at: None,
    };
    for (status, reason, ok) in [
        (200, "OK", true),
        (404, "Not Found", true),
        (500, "Boom", false),
    ] {
        let (url, seen) = spawn_mock_recorder(status, reason, b"{}".to_vec()).await;
        let endpoint = Endpoint::new(reqwest::Client::new(), &url, Auth::Key("k".into()));
        assert_eq!(
            delete_openai_shape(&endpoint, &file).await.is_ok(),
            ok,
            "{status}"
        );
        assert!(
            seen.lock()
                .unwrap()
                .join("")
                .contains("DELETE /files/file-9")
        );
    }
}

#[tokio::test]
async fn an_unreachable_host_fails_an_upload_and_a_delete() {
    let endpoint = Endpoint::new(
        reqwest::Client::new(),
        "http://127.0.0.1:9",
        Auth::Key("k".into()),
    );
    assert!(
        upload_openai_shape(
            &endpoint,
            &upload("application/pdf"),
            "x",
            &provider_limits("xai")
        )
        .await
        .is_err()
    );
    let file = RemoteFile {
        id: "f".into(),
        uri: None,
        expires_at: None,
    };
    assert!(delete_openai_shape(&endpoint, &file).await.is_err());
}
