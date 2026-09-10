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
use axum::http::StatusCode;
use leviath_core::mime::inline_refs::extract;
use leviath_core::mime::{Delivery, InboundPart, MimeType};
use serde::{Deserialize, Serialize};

use super::types::{ApiError, err};

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
fn delivery(word: &str) -> Result<Delivery, ApiError> {
    match word {
        "native" => Ok(Delivery::Native),
        "text" => Ok(Delivery::Text),
        "stand_in" => Ok(Delivery::StandIn),
        other => Err(err(
            StatusCode::BAD_REQUEST,
            format!("deliver must be native, text or stand_in, not '{other}'"),
        )),
    }
}

/// A file inside `workdir` as a part, refused when the path escapes, the
/// file is missing or empty, or the file is over `max_bytes`.
fn read_within(path: &str, workdir: &Path, max_bytes: u64) -> Result<InboundPart, ApiError> {
    let full = workdir.join(path);
    if !leviath_core::resolves_within(&full, workdir) {
        return Err(err(
            StatusCode::FORBIDDEN,
            format!("part path '{path}' is outside the run's working directory"),
        ));
    }
    let data = std::fs::read(&full).map_err(|e| {
        err(
            StatusCode::BAD_REQUEST,
            format!("part '{path}' could not be read: {e}"),
        )
    })?;
    if data.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("part '{path}' is empty"),
        ));
    }
    if data.len() as u64 > max_bytes {
        return Err(err(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "part '{path}' is {} bytes, over the {max_bytes} byte ceiling",
                data.len()
            ),
        ));
    }
    let name = full
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(InboundPart::from_bytes(name, data))
}

/// Every entry of a JSON `parts` list, read from the workdir.
pub(super) fn json_parts(
    listed: &[PartRef],
    workdir: &Path,
    max_bytes: u64,
) -> Result<Vec<InboundPart>, ApiError> {
    let mut parts = Vec::with_capacity(listed.len());
    for item in listed {
        let mut part = read_within(&item.path, workdir, max_bytes)?;
        part.region = item.region.clone();
        if let Some(name) = &item.name {
            part.name = name.clone();
        }
        if let Some(t) = &item.mime_type {
            part.mime_type = Some(MimeType::parse(t).map_err(|e| {
                err(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "part '{}' has mime_type '{t}', which is not one: {e}",
                        item.path
                    ),
                )
            })?);
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
    max_bytes: u64,
) -> Result<(String, Vec<InboundPart>), ApiError> {
    let extracted = extract(text, &mut |path| {
        let full = workdir.join(path);
        leviath_core::resolves_within(&full, workdir) && full.is_file()
    });
    let mut parts = Vec::new();
    for r in &extracted.refs {
        let mut part = read_within(&r.path, workdir, max_bytes)?;
        if let Some(t) = &r.mime_type {
            part.mime_type = Some(t.clone());
        }
        part.region = region.map(str::to_string);
        parts.push(part);
    }
    Ok((extracted.text, parts))
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

/// Read a multipart body: one `request` field of JSON, and files named
/// `part` (bound for the task region) or `part:<region>`.
pub(super) async fn read_multipart(
    mut multipart: Multipart,
    max_bytes: u64,
) -> Result<MultipartBody, ApiError> {
    let mut request = None;
    let mut parts = Vec::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    format!("malformed multipart body: {e}"),
                ));
            }
        };
        let name = field.name().unwrap_or_default().to_string();
        if name == "request" {
            let text = field.text().await.map_err(|e| {
                err(
                    StatusCode::BAD_REQUEST,
                    format!("the request field could not be read: {e}"),
                )
            })?;
            request = Some(serde_json::from_str(&text).map_err(|e| {
                err(
                    StatusCode::BAD_REQUEST,
                    format!("the request field is not JSON: {e}"),
                )
            })?);
            continue;
        }
        let Some(region) = name
            .strip_prefix("part")
            .map(|rest| rest.strip_prefix(':').filter(|r| !r.is_empty()))
        else {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!(
                    "unexpected multipart field '{name}'; send `request` and files named `part` \
                     or `part:<region>`"
                ),
            ));
        };
        let region = region.map(str::to_string);
        let file_name = field
            .file_name()
            .map(str::to_string)
            .filter(|f| !f.is_empty());
        let declared = field
            .content_type()
            .and_then(|t| MimeType::parse(t).ok())
            .filter(|t| t.as_str() != "application/octet-stream");
        let data = field.bytes().await.map_err(|e| {
            err(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("a file part could not be read: {e}"),
            )
        })?;
        if data.is_empty() {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "a file part is empty".to_string(),
            ));
        }
        if data.len() as u64 > max_bytes {
            return Err(err(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "file part '{}' is {} bytes, over the {max_bytes} byte ceiling",
                    file_name.clone().unwrap_or_default(),
                    data.len()
                ),
            ));
        }
        let name = file_name.unwrap_or_else(|| format!("part-{}", parts.len() + 1));
        let mut part = InboundPart::from_bytes(name, data.to_vec());
        part.region = region;
        part.mime_type = declared;
        parts.push(part);
    }
    let Some(request) = request else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "a multipart body needs a `request` field holding the JSON request".to_string(),
        ));
    };
    Ok(MultipartBody { request, parts })
}

/// The route's body, whichever way it came: a JSON body as the route's
/// type with no files, or a multipart body's `request` field as that type
/// with the files it carried.
pub(super) async fn json_or_multipart<T: serde::de::DeserializeOwned, S: Send + Sync>(
    state: &S,
    request: axum::extract::Request,
    max_bytes: u64,
) -> Result<(T, Vec<InboundPart>), ApiError> {
    use axum::extract::FromRequest;
    let is_multipart = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("multipart/form-data"));
    if is_multipart {
        let multipart = Multipart::from_request(request, state)
            .await
            .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e}")))?;
        let body = read_multipart(multipart, max_bytes).await?;
        return Ok((body_from(body.request)?, body.parts));
    }
    let axum::Json(value) = axum::Json::<serde_json::Value>::from_request(request, state)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.body_text()))?;
    Ok((body_from(value)?, Vec::new()))
}

/// Parse `value` as the route's JSON body, wording a failure the way the
/// JSON extractor does.
pub(super) fn body_from<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, ApiError> {
    serde_json::from_value(value)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid request: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let parts = json_parts(&listed, dir.path(), 1024).unwrap();
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
        let parts = json_parts(&bare, dir.path(), 1024).unwrap();
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
            json_parts(&listed, dir.path(), max).unwrap_err().0
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
            1024,
        )
        .unwrap();
        assert_eq!(
            text,
            "edit @hero.png and @literal, not @../etc/passwd or @channel"
        );
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "hero.png");
        assert_eq!(parts[0].region.as_deref(), Some("task"));
        let (_, typed) = inline_parts("@hero.png:image/webp", None, dir.path(), 1024).unwrap();
        assert_eq!(typed[0].mime_type.as_ref().unwrap().as_str(), "image/webp");
        assert!(typed[0].region.is_none());
        assert_eq!(
            inline_parts("@hero.png", None, dir.path(), 4)
                .unwrap_err()
                .0,
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
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(body_from::<Shape>(serde_json::json!({"_n": 1})).is_ok());
    }

    /// Read `body` as a form whose boundary is `b`.
    async fn form(body: Vec<u8>, max: u64) -> Result<MultipartBody, ApiError> {
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
            --b--\r\n";
        let body = form(ok.to_vec(), 1024).await.unwrap();
        assert_eq!(body.request["k"], 1);
        assert_eq!(body.parts.len(), 2);
        // No file name: numbered. An octet-stream type: no type at all, so
        // the daemon sniffs one.
        assert_eq!(body.parts[0].name, "part-1");
        assert_eq!(body.parts[0].mime_type, None);
        assert_eq!(body.parts[0].region, None);
        // `part:` with nothing after the colon is the task region.
        assert_eq!(body.parts[1].name, "a.bin");
        assert_eq!(body.parts[1].region, None);

        let over = form(ok.to_vec(), 2).await.unwrap_err();
        assert_eq!(over.0, StatusCode::PAYLOAD_TOO_LARGE);
        let garbage = form(b"garbage".to_vec(), 1024).await.unwrap_err();
        assert_eq!(garbage.0, StatusCode::BAD_REQUEST);
        // A stream that ends inside the request field, and one that ends
        // inside a file.
        let cut_request =
            b"--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{}".to_vec();
        assert_eq!(
            form(cut_request, 1024).await.unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        let cut_file = b"--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{}\r\n\
            --b\r\nContent-Disposition: form-data; name=\"part\"; filename=\"a\"\r\n\r\nxy"
            .to_vec();
        assert_eq!(
            form(cut_file, 1024).await.unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
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
        assert_eq!(e.0, StatusCode::BAD_REQUEST);
        let no_form = request("multipart/form-data; boundary=b", "garbage");
        let e = json_or_multipart::<Shape, ()>(&(), no_form, 1024)
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST);
        // JSON that is not JSON, and a form whose request is the wrong shape.
        let not_json = request("application/json", "nope");
        let e = json_or_multipart::<Shape, ()>(&(), not_json, 1024)
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST);
        let wrong_shape = request(
            "multipart/form-data; boundary=b",
            "--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{\"k\":\"x\"}\r\n--b--\r\n",
        );
        let e = json_or_multipart::<Shape, ()>(&(), wrong_shape, 1024)
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST);
        let json_wrong_shape = request("application/json", "{\"k\":\"x\"}");
        let e = json_or_multipart::<Shape, ()>(&(), json_wrong_shape, 1024)
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST);
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
