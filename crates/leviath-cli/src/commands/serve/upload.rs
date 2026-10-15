//! Files arriving over HTTP: `multipart/form-data` on the spawn and message
//! routes, the JSON `parts` list naming files already inside the workdir,
//! and `@path` tokens inside a task or message.
//!
//! A multipart body carries a `request` field holding the JSON the route
//! takes as a plain body, then any number of file fields named `part` or
//! `part:<region>`, each with a `filename` and a `Content-Type`. A JSON body
//! instead carries `parts: [{ region?, name?, mime_type?, path, deliver?,
//! caption? }]`, each `path` resolved inside the run's working directory.
//! Both end as [`InboundPart`]s on the control request, exactly what
//! `lev run --attach` sends.

use std::path::Path;

use axum::extract::multipart::Multipart;
use leviath_core::mime::{Delivery, InboundPart, MimeType};
use serde::{Deserialize, Serialize};

use super::core::attachments::{self, FileBudget};
use super::core::error::ServeError;

/// One entry of a JSON `parts` list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct PartRef {
    /// The region to put it in; the task region when absent.
    #[serde(default)]
    pub(super) region: Option<String>,
    /// The name the part carries; the file name when absent.
    #[serde(default)]
    pub(super) name: Option<String>,
    /// Its mime type, when the bytes and name do not say.
    #[serde(default)]
    pub(super) mime_type: Option<String>,
    /// The file, relative to the run's working directory.
    pub(super) path: String,
    /// How it reaches the model: `native`, `text` or `stand_in`.
    #[serde(default)]
    pub(super) deliver: Option<String>,
    /// Text stored beside it.
    #[serde(default)]
    pub(super) caption: Option<String>,
}

/// The `deliver` word a request used, as the runtime's choice.
fn delivery(word: &str) -> Result<Delivery, ServeError> {
    attachments::delivery(word)
}

/// A file inside `workdir` as a part, refused when the path escapes, the
/// file is missing, empty or not a file, or it is over what `budget` has
/// left.
///
/// The reading itself is [`attachments::read_within`], which GraphQL's
/// `attachments` lists call too; this wraps its refusal in the status and body
/// a REST client reads.
fn read_within(path: &str, workdir: &Path, budget: &FileBudget) -> Result<InboundPart, ServeError> {
    attachments::read_within(path, workdir, budget)
}

/// Every entry of a JSON `parts` list, read from the workdir.
pub(super) fn json_parts(
    listed: &[PartRef],
    workdir: &Path,
    budget: &FileBudget,
) -> Result<Vec<InboundPart>, ServeError> {
    let mut parts = Vec::with_capacity(listed.len());
    for item in listed {
        let mut part = read_within(&item.path, workdir, budget)?;
        part.region = item.region.clone();
        if let Some(name) = &item.name {
            part.name = name.clone();
        }
        if let Some(t) = &item.mime_type {
            part.mime_type = Some(attachments::mime_type(&item.path, t)?);
        }
        if let Some(d) = &item.deliver {
            part.deliver = Some(delivery(d)?);
        }
        part.caption = item.caption.clone();
        parts.push(part);
    }
    Ok(parts)
}

/// The parts a text names with `@path`, each a file inside `workdir`, bound
/// for `region`. The text comes back with only `\@` unescaped; a token that
/// names no file is left as text, as on the command line.
pub(super) fn inline_parts(
    text: &str,
    region: Option<&str>,
    workdir: &Path,
    budget: &FileBudget,
) -> Result<(String, Vec<InboundPart>), ServeError> {
    let (text, parts, unresolved) = leviath_core::mime::inline_refs::parts_from_text(
        text,
        region,
        &mut |path| {
            let full = workdir.join(path);
            leviath_core::resolves_within(&full, workdir) && full.is_file()
        },
        &mut |path| read_within(path, workdir, budget),
    )?;
    // The command line warns the person typing; over HTTP the daemon's log is
    // where a typo'd path can be seen, since the request still goes through.
    if !unresolved.is_empty() {
        tracing::warn!(
            tokens = ?unresolved,
            "@path tokens named no file inside the working directory and were left as text"
        );
    }
    Ok((text, parts))
}

/// A multipart body, split into the JSON the route takes and the files it
/// carried.
#[derive(Debug)]
pub(super) struct MultipartBody {
    /// The `request` field's JSON.
    pub(super) request: serde_json::Value,
    /// Every `part` field as an inbound part.
    pub(super) parts: Vec<InboundPart>,
}

/// One field of a multipart body other than `request`: a file.
#[derive(Debug)]
pub(super) struct FileField {
    /// The field's name, which each route reads its own way.
    pub(super) name: String,
    /// The file name it was sent with, when it was sent with one.
    pub(super) file_name: Option<String>,
    /// The type it was sent as, without parameters, other than the catch-all
    /// `application/octet-stream`, which says nothing.
    pub(super) content_type: Option<String>,
    /// Its bytes.
    pub(super) data: Vec<u8>,
}

/// A body the extractor could not read, as the failure it is: one over the
/// size limit, or one that is not well formed.
pub(super) fn unreadable(status: axum::http::StatusCode, message: String) -> ServeError {
    match status {
        axum::http::StatusCode::PAYLOAD_TOO_LARGE => ServeError::PayloadTooLarge(message),
        _ => ServeError::BadRequest(message),
    }
}

/// A file field sent with a `Content-Type` that is not a mime type.
///
/// Refused rather than sniffed: the client said what the bytes are, and
/// quietly reading them as something else would hide its mistake.
pub(super) fn bad_type(field: &str, sent: &str, why: impl std::fmt::Display) -> ServeError {
    ServeError::BadRequest(format!(
        "file field '{field}' was sent as '{sent}', which is not a mime type: {why}"
    ))
}

/// Read a multipart body's envelope: exactly one `request` field of JSON,
/// and every other field as a file of at most `max_bytes`. What the file
/// fields may be named is the route's to judge.
pub(super) async fn read_envelope(
    mut multipart: Multipart,
    max_bytes: u64,
) -> Result<(serde_json::Value, Vec<FileField>), ServeError> {
    let mut request = None;
    let mut files = Vec::new();
    loop {
        let field = multipart
            .next_field()
            .await
            .map_err(|e| unreadable(e.status(), format!("malformed multipart body: {e}")))?;
        let Some(field) = field else { break };
        let name = field.name().unwrap_or_default().to_string();
        if name == "request" {
            if request.is_some() {
                return Err(ServeError::BadRequest(
                    "a multipart body carries one `request` field, and this one carries more"
                        .to_string(),
                ));
            }
            let text = field.text().await.map_err(|e| {
                unreadable(
                    e.status(),
                    format!("the request field could not be read: {e}"),
                )
            })?;
            request = Some(serde_json::from_str(&text).map_err(|e| {
                ServeError::BadRequest(format!("the request field is not JSON: {e}"))
            })?);
            continue;
        }
        let file_name = field
            .file_name()
            .map(str::to_string)
            .filter(|f| !f.is_empty());
        // The `type/subtype` alone: a browser sends a text blob as
        // `text/plain;charset=utf-8`, and the parameters say nothing the
        // registry keeps.
        let content_type = field
            .content_type()
            .map(|t| t.split(';').next().unwrap_or(t).trim())
            .filter(|t| *t != "application/octet-stream")
            .map(str::to_string);
        // A body over the server's limit, or one that ends mid-file: the
        // error says which.
        let data = field.bytes().await.map_err(|e| {
            unreadable(
                e.status(),
                format!("file field '{name}' could not be read: {e}"),
            )
        })?;
        if data.len() as u64 > max_bytes {
            return Err(ServeError::PayloadTooLarge(format!(
                "file field '{name}' is {} bytes, over the {max_bytes} byte ceiling",
                data.len()
            )));
        }
        files.push(FileField {
            name,
            file_name,
            content_type,
            data: data.to_vec(),
        });
    }
    let request = request.ok_or_else(|| {
        ServeError::BadRequest(
            "a multipart body needs a `request` field holding the JSON request".to_string(),
        )
    })?;
    Ok((request, files))
}

/// Read a multipart body: one `request` field of JSON, and files named
/// `part` (bound for the task region) or `part:<region>`.
pub(super) async fn read_multipart(
    multipart: Multipart,
    max_bytes: u64,
) -> Result<MultipartBody, ServeError> {
    let (request, files) = read_envelope(multipart, max_bytes).await?;
    let mut parts = Vec::with_capacity(files.len());
    for file in files {
        let region = part_region(&file.name)?;
        if file.data.is_empty() {
            return Err(ServeError::BadRequest(format!(
                "file field '{}' is empty",
                file.name
            )));
        }
        let name = file
            .file_name
            .unwrap_or_else(|| format!("part-{}", parts.len() + 1));
        let mime_type = file
            .content_type
            .as_deref()
            .map(|t| MimeType::parse(t).map_err(|e| bad_type(&file.name, t, e)))
            .transpose()?;
        let mut part = InboundPart::from_bytes(name, file.data);
        part.region = region;
        part.mime_type = mime_type;
        parts.push(part);
    }
    Ok(MultipartBody { request, parts })
}

/// The region a file field named `part` or `part:<region>` is for: `None`,
/// the task region, for a bare `part` (or `part:` with no region after it).
/// Any other name is refused, a near miss such as `parts` or `partition`
/// included.
fn part_region(name: &str) -> Result<Option<String>, ServeError> {
    if name == "part" {
        return Ok(None);
    }
    let region = name.strip_prefix("part:").ok_or_else(|| {
        ServeError::BadRequest(format!(
            "unexpected multipart field '{name}'; send `request` and files named `part` or \
             `part:<region>`"
        ))
    })?;
    Ok((!region.is_empty()).then(|| region.to_string()))
}

/// The route's body, whichever way it came: a JSON body as the route's
/// type with no files, or a multipart body's `request` field as that type
/// with the files it carried.
pub(super) async fn json_or_multipart<T: serde::de::DeserializeOwned, S: Send + Sync>(
    state: &S,
    request: axum::extract::Request,
    max_bytes: u64,
) -> Result<(T, Vec<InboundPart>), ServeError> {
    use axum::extract::FromRequest;
    let is_multipart = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("multipart/form-data"));
    if is_multipart {
        let multipart = Multipart::from_request(request, state)
            .await
            .map_err(|e| ServeError::BadRequest(format!("{e}")))?;
        let body = read_multipart(multipart, max_bytes).await?;
        return Ok((body_from(body.request)?, body.parts));
    }
    let axum::Json(value) = axum::Json::<serde_json::Value>::from_request(request, state)
        .await
        .map_err(|e| unreadable(e.status(), e.body_text()))?;
    Ok((body_from(value)?, Vec::new()))
}

/// Parse `value` as the route's JSON body, wording a failure the way the
/// JSON extractor does.
pub(super) fn body_from<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, ServeError> {
    serde_json::from_value(value)
        .map_err(|e| ServeError::BadRequest(format!("invalid request: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn png() -> Vec<u8> {
        b"\x89PNG\r\n\x1a\nbody".to_vec()
    }

    #[test]
    fn json_parts_are_read_inside_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hero.png"), png()).unwrap();
        std::fs::write(dir.path().join("empty.png"), b"").unwrap();
        let listed = vec![PartRef {
            region: Some("art".to_string()),
            name: Some("the-hero".to_string()),
            mime_type: Some("image/png".to_string()),
            path: "hero.png".to_string(),
            deliver: Some("text".to_string()),
            caption: Some("v1".to_string()),
        }];
        let parts = json_parts(&listed, dir.path(), &FileBudget::new(1024)).unwrap();
        assert_eq!(parts[0].name, "the-hero");
        assert_eq!(parts[0].region.as_deref(), Some("art"));
        assert_eq!(parts[0].mime_type.as_ref().unwrap().as_str(), "image/png");
        assert_eq!(parts[0].deliver, Some(Delivery::Text));
        assert_eq!(parts[0].caption.as_deref(), Some("v1"));
        let bare = vec![PartRef {
            region: None,
            name: None,
            mime_type: None,
            path: "hero.png".to_string(),
            deliver: None,
            caption: None,
        }];
        let parts = json_parts(&bare, dir.path(), &FileBudget::new(1024)).unwrap();
        assert_eq!(parts[0].name, "hero.png");
        assert_eq!(parts[0].mime_type, None);

        let refused = |path: &str, mime_type: Option<&str>, deliver: Option<&str>, max: u64| {
            let listed = vec![PartRef {
                region: None,
                name: None,
                mime_type: mime_type.map(str::to_string),
                path: path.to_string(),
                deliver: deliver.map(str::to_string),
                caption: None,
            }];
            json_parts(&listed, dir.path(), &FileBudget::new(max))
                .unwrap_err()
                .status()
        };
        assert_eq!(refused("../x.png", None, None, 1024), StatusCode::FORBIDDEN);
        assert_eq!(
            refused("missing.png", None, None, 1024),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            refused("empty.png", None, None, 1024),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            refused("hero.png", None, None, 4),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            refused("hero.png", Some("nope"), None, 1024),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            refused("hero.png", None, Some("loud"), 1024),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(delivery("native").unwrap(), Delivery::Native);
        assert_eq!(delivery("stand_in").unwrap(), Delivery::StandIn);
    }

    #[test]
    fn inline_references_resolve_inside_the_workdir_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hero.png"), png()).unwrap();
        let (text, parts) = inline_parts(
            "edit @hero.png and \\@literal, not @../etc/passwd or @channel",
            Some("task"),
            dir.path(),
            &FileBudget::new(1024),
        )
        .unwrap();
        assert_eq!(
            text,
            "edit @hero.png and @literal, not @../etc/passwd or @channel"
        );
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "hero.png");
        assert_eq!(parts[0].region.as_deref(), Some("task"));
        let (_, typed) = inline_parts(
            "@hero.png:image/webp",
            None,
            dir.path(),
            &FileBudget::new(1024),
        )
        .unwrap();
        assert_eq!(typed[0].mime_type.as_ref().unwrap().as_str(), "image/webp");
        assert!(typed[0].region.is_none());
        assert_eq!(
            inline_parts("@hero.png", None, dir.path(), &FileBudget::new(4))
                .unwrap_err()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn a_body_that_is_not_the_route_shape_is_a_bad_request() {
        #[derive(Debug, Deserialize)]
        struct Shape {
            _n: u32,
        }
        let err = body_from::<Shape>(serde_json::json!({"n": "x"})).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(body_from::<Shape>(serde_json::json!({"_n": 1})).is_ok());
    }

    /// Read `body` as a form whose boundary is `b`.
    async fn form(body: Vec<u8>, max: u64) -> Result<MultipartBody, ServeError> {
        use axum::extract::FromRequest;
        let request = axum::extract::Request::builder()
            .header("content-type", "multipart/form-data; boundary=b")
            .body(axum::body::Body::from(body))
            .unwrap();
        let multipart = Multipart::from_request(request, &()).await.unwrap();
        read_multipart(multipart, max).await
    }

    #[tokio::test]
    async fn a_form_names_what_it_lacks_and_refuses_what_it_cannot_take() {
        let ok = b"--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{\"k\":1}\r\n\
            --b\r\nContent-Disposition: form-data; name=\"part\"\r\n\
            Content-Type: application/octet-stream\r\n\r\nxyz\r\n\
            --b\r\nContent-Disposition: form-data; name=\"part:\"; filename=\"a.bin\"\r\n\r\nqq\r\n\
            --b\r\nContent-Disposition: form-data; name=\"part:art\"; filename=\"b.bin\"\r\n\r\nzz\r\n\
            --b--\r\n";
        let body = form(ok.to_vec(), 1024).await.unwrap();
        assert_eq!(body.request["k"], 1);
        assert_eq!(body.parts.len(), 3);
        assert_eq!(body.parts[2].region.as_deref(), Some("art"));
        // A type with parameters keeps its `type/subtype`.
        let with_charset = b"--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{}\r\n\
            --b\r\nContent-Disposition: form-data; name=\"part\"; filename=\"t\"\r\n\
            Content-Type: text/plain; charset=utf-8\r\n\r\nxyz\r\n--b--\r\n";
        let typed = form(with_charset.to_vec(), 1024).await.unwrap();
        assert_eq!(
            typed.parts[0].mime_type.as_ref().map(MimeType::as_str),
            Some("text/plain")
        );
        // No file name: numbered. An octet-stream type: no type at all, so
        // the daemon sniffs one.
        assert_eq!(body.parts[0].name, "part-1");
        assert_eq!(body.parts[0].mime_type, None);
        assert_eq!(body.parts[0].region, None);
        // `part:` with nothing after the colon is the task region.
        assert_eq!(body.parts[1].name, "a.bin");
        assert_eq!(body.parts[1].region, None);

        let over = form(ok.to_vec(), 2).await.unwrap_err();
        assert_eq!(over.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let garbage = form(b"garbage".to_vec(), 1024).await.unwrap_err();
        assert_eq!(garbage.status(), StatusCode::BAD_REQUEST);
        // A stream that ends inside the request field, and one that ends
        // inside a file: malformed, not too large.
        let cut_request =
            b"--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{}".to_vec();
        assert_eq!(
            form(cut_request, 1024).await.unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
        let cut_file = b"--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{}\r\n\
            --b\r\nContent-Disposition: form-data; name=\"part\"; filename=\"a\"\r\n\r\nxy"
            .to_vec();
        assert_eq!(
            form(cut_file, 1024).await.unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
        // A request that is not JSON, a field that is neither the request nor
        // a part, an empty part, a named part over the ceiling, and no request
        // at all.
        let refused = [
            (
                b"--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{\r\n--b--\r\n"
                    .to_vec(),
                1024,
                StatusCode::BAD_REQUEST,
            ),
            (
                b"--b\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nx\r\n--b--\r\n"
                    .to_vec(),
                1024,
                StatusCode::BAD_REQUEST,
            ),
            (
                b"--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{}\r\n\
                  --b\r\nContent-Disposition: form-data; name=\"part\"; filename=\"e\"\r\n\r\n\r\n--b--\r\n"
                    .to_vec(),
                1024,
                StatusCode::BAD_REQUEST,
            ),
            (
                b"--b\r\nContent-Disposition: form-data; name=\"part\"; filename=\"big\"\r\n\r\nxyz\r\n--b--\r\n"
                    .to_vec(),
                2,
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
            (
                b"--b\r\nContent-Disposition: form-data; name=\"part\"; filename=\"a\"\r\n\r\nxyz\r\n--b--\r\n"
                    .to_vec(),
                1024,
                StatusCode::BAD_REQUEST,
            ),
            // A part sent as a type the registry cannot spell: refused, not
            // sniffed.
            (
                b"--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{}\r\n\
                  --b\r\nContent-Disposition: form-data; name=\"part\"; filename=\"a\"\r\n\
                  Content-Type: text/x|y\r\n\r\nxyz\r\n--b--\r\n"
                    .to_vec(),
                1024,
                StatusCode::BAD_REQUEST,
            ),
        ];
        for (body, max, status) in refused {
            assert_eq!(form(body, max).await.unwrap_err().status(), status);
        }
    }

    /// Only `part` and `part:<region>` name a file: a near miss is refused
    /// rather than taken as a part for the task. A second `request` field is
    /// refused rather than quietly replacing the first.
    #[tokio::test]
    async fn near_miss_field_names_and_a_second_request_are_refused() {
        let request = "--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{}\r\n";
        for name in ["partition", "parts", "partX", "part-a"] {
            let body = format!(
                "{request}--b\r\nContent-Disposition: form-data; name=\"{name}\"; \
                 filename=\"a\"\r\n\r\nxyz\r\n--b--\r\n"
            );
            let refused = form(body.into_bytes(), 1024).await.unwrap_err();
            assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{name}");
            assert!(refused.to_string().contains(name), "{refused}");
        }
        let twice = format!("{request}{request}--b--\r\n");
        let refused = form(twice.into_bytes(), 1024).await.unwrap_err();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert!(
            refused.to_string().contains("one `request` field"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn a_route_body_is_json_or_a_form() {
        #[derive(Debug, Deserialize)]
        struct Shape {
            k: u32,
        }
        let request = |content_type: &str, body: &'static str| {
            axum::extract::Request::builder()
                .header("content-type", content_type)
                .body(axum::body::Body::from(body))
                .unwrap()
        };
        let (shape, parts): (Shape, _) =
            json_or_multipart(&(), request("application/json", "{\"k\":1}"), 1024)
                .await
                .unwrap();
        assert_eq!(shape.k, 1);
        assert!(parts.is_empty());
        // A form with no boundary, and one with a boundary and no form.
        let no_boundary = request("multipart/form-data", "x");
        let e = json_or_multipart::<Shape, ()>(&(), no_boundary, 1024)
            .await
            .unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        let no_form = request("multipart/form-data; boundary=b", "garbage");
        let e = json_or_multipart::<Shape, ()>(&(), no_form, 1024)
            .await
            .unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        // JSON that is not JSON, and a form whose request is the wrong shape.
        let not_json = request("application/json", "nope");
        let e = json_or_multipart::<Shape, ()>(&(), not_json, 1024)
            .await
            .unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        let wrong_shape = request(
            "multipart/form-data; boundary=b",
            "--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{\"k\":\"x\"}\r\n--b--\r\n",
        );
        let e = json_or_multipart::<Shape, ()>(&(), wrong_shape, 1024)
            .await
            .unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        let json_wrong_shape = request("application/json", "{\"k\":\"x\"}");
        let e = json_or_multipart::<Shape, ()>(&(), json_wrong_shape, 1024)
            .await
            .unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        // JSON over the body limit is too large, as a form over it is, not
        // malformed.
        let padding = "x".repeat(3 * 1024 * 1024);
        let too_big = axum::extract::Request::builder()
            .header("content-type", "application/json")
            .body(axum::body::Body::from(format!(
                "{{\"k\":1,\"pad\":\"{padding}\"}}"
            )))
            .unwrap();
        let e = json_or_multipart::<Shape, ()>(&(), too_big, 1024)
            .await
            .unwrap_err();
        assert_eq!(e.status(), StatusCode::PAYLOAD_TOO_LARGE);
        // A whole form: the request and its file both come through.
        let whole = request(
            "multipart/form-data; boundary=b",
            "--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{\"k\":2}\r\n\
             --b\r\nContent-Disposition: form-data; name=\"part\"; filename=\"a.png\"\r\n\r\nxyz\r\n\
             --b--\r\n",
        );
        let (shape, parts): (Shape, _) = json_or_multipart(&(), whole, 1024).await.unwrap();
        assert_eq!(shape.k, 2);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "a.png");
    }
}
