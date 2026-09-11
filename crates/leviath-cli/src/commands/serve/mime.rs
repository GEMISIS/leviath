//! Writing a mime row over the API.
//!
//! `GET /api/mime` (in [`super::blobs`]) reads the effective registry, but
//! nothing wrote to it, so a console could show a custom type and never make
//! one - and a custom type is the point of the registry: the family, token
//! rule, extensions and check that lift a format Leviath has never heard of
//! out of behaving like `application/octet-stream`. `PUT /api/mime` adds or
//! updates a row and `DELETE /api/mime` takes one out, both admin-gated and
//! both writing `mime_types.toml` beside the config - the same file and the
//! same validation `lev mime add` and `lev mime remove` use, so the rows the
//! browser writes and the rows the operator writes are one set in one place.

use axum::Json;
use axum::extract::Query;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use leviath_core::mime::MimeType;

use super::types::*;
use crate::commands::mime_rows::{Added, RowEdit, TokenSpec, add_row, remove_row};

/// A token rule as JSON: exactly one of the four rates, `max` only beside
/// `per_pixel`, the shape a `[mime_types]` row's `tokens` table takes.
#[derive(Debug, Deserialize)]
pub(super) struct TokenRuleReq {
    /// `{ per_byte = 0.25 }`.
    per_byte: Option<f64>,
    /// `{ per_pixel = 750 }`, with an optional `max`.
    per_pixel: Option<i64>,
    /// `{ per_second = 32 }`.
    per_second: Option<i64>,
    /// `{ fixed = 1000 }`.
    fixed: Option<i64>,
    /// The cap, only alongside `per_pixel`.
    max: Option<i64>,
}

impl TokenRuleReq {
    /// The rule this describes, or the reason it is not one.
    fn into_spec(self) -> Result<TokenSpec, String> {
        let named = [
            self.per_byte.is_some(),
            self.per_pixel.is_some(),
            self.per_second.is_some(),
            self.fixed.is_some(),
        ]
        .iter()
        .filter(|set| **set)
        .count();
        if named != 1 {
            return Err("name exactly one of per_byte, per_pixel, per_second, fixed".into());
        }
        if self.max.is_some() && self.per_pixel.is_none() {
            return Err("max only goes with per_pixel".into());
        }
        if let Some(rate) = self.per_byte {
            Ok(TokenSpec::PerByte(rate))
        } else if let Some(divisor) = self.per_pixel {
            Ok(TokenSpec::PerPixel {
                divisor,
                max: self.max,
            })
        } else if let Some(rate) = self.per_second {
            Ok(TokenSpec::PerSecond(rate))
        } else {
            Ok(TokenSpec::Fixed(self.fixed.unwrap_or_default()))
        }
    }
}

/// Body of `PUT /api/mime`: the type to write and the fields to set on its row,
/// each field optional so a caller changes only what it names.
#[derive(Debug, Deserialize)]
pub(super) struct MimeRowReq {
    /// `type/subtype`, or `type/*` for a whole family.
    mime_type: String,
    /// `family`.
    family: Option<String>,
    /// `text`.
    text: Option<bool>,
    /// `tokens`.
    tokens: Option<TokenRuleReq>,
    /// `extensions`.
    extensions: Option<Vec<String>>,
    /// `magic`, a hex prefix.
    magic: Option<String>,
    /// `stand_in` template.
    stand_in: Option<String>,
    /// `check`, a Rhai script path; an empty string lifts a broader row's check.
    check: Option<String>,
}

/// What `PUT /api/mime` answers with.
#[derive(Debug, Serialize)]
pub(super) struct MimeRowWritten {
    /// The type the row is for, as it was parsed.
    mime_type: String,
    /// Whether the row was new, rather than an update of one already there.
    created: bool,
}

/// Query for `DELETE /api/mime`: the type to take out.
#[derive(Debug, Deserialize)]
pub(super) struct DeleteMimeQuery {
    /// `type/subtype` or `type/*`.
    mime_type: String,
}

/// The operator's `mime_types.toml`, beside the config this process edits.
fn mime_types_path() -> std::path::PathBuf {
    super::mcp::admin_paths()
        .config
        .with_file_name("mime_types.toml")
}

/// `PUT /api/mime` (admin-only): add a row to `mime_types.toml`, or set the
/// given fields on the one already there. Validated the way `lev mime add` is -
/// a bad type, a bad token rule, or a `check` that will not compile is a 400
/// and nothing is written.
pub(super) async fn put_mime_row(
    Json(req): Json<MimeRowReq>,
) -> Result<Json<MimeRowWritten>, ApiError> {
    let key = MimeType::parse(&req.mime_type)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{}: {e}", req.mime_type)))?;
    let tokens = match req.tokens {
        Some(t) => Some(t.into_spec().map_err(|e| err(StatusCode::BAD_REQUEST, e))?),
        None => None,
    };
    let edit = RowEdit {
        family: req.family,
        text: req.text,
        tokens,
        extensions: req.extensions,
        magic: req.magic,
        stand_in: req.stand_in,
        check: req.check,
    };
    let path = mime_types_path();
    let added = add_row(&path, key.as_str(), &edit).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(MimeRowWritten {
        mime_type: key.as_str().to_string(),
        created: added == Added::Created,
    }))
}

/// `DELETE /api/mime?mime_type=...` (admin-only): take a row out of
/// `mime_types.toml`. A type that has no row there is a 404.
pub(super) async fn delete_mime_row(
    Query(q): Query<DeleteMimeQuery>,
) -> Result<StatusCode, ApiError> {
    let key = MimeType::parse(&q.mime_type)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{}: {e}", q.mime_type)))?;
    let path = mime_types_path();
    remove_row(&path, key.as_str()).map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::serve::mcp::{AdminPaths, scoped};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::put;
    use tower::ServiceExt;

    /// A router mounting the two write handlers, scoped to a `mime_types.toml`
    /// beside `config` so `admin_paths()` resolves there.
    fn app(config: std::path::PathBuf) -> Router {
        let paths = AdminPaths {
            store: config.with_file_name("mcp-auth.json"),
            grants: config.with_file_name("provider-auth.json"),
            config,
        };
        scoped(
            Router::new().route("/api/mime", put(put_mime_row).delete(delete_mime_row)),
            paths,
        )
    }

    async fn put_row(dir: &std::path::Path, body: serde_json::Value) -> StatusCode {
        let req = Request::builder()
            .method("PUT")
            .uri("/api/mime")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app(dir.join("config.toml"))
            .oneshot(req)
            .await
            .unwrap()
            .status()
    }

    async fn delete_row(dir: &std::path::Path, mime_type: &str) -> StatusCode {
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/mime?mime_type={mime_type}"))
            .body(Body::empty())
            .unwrap();
        app(dir.join("config.toml"))
            .oneshot(req)
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_row_is_written_then_updated_then_removed() {
        let dir = tempfile::tempdir().unwrap();
        // Created: a new row with every field kind, a per_pixel rule with a cap.
        let status = put_row(
            dir.path(),
            serde_json::json!({
                "mime_type": "application/x-acme-scene",
                "family": "model",
                "text": false,
                "tokens": { "per_pixel": 750, "max": 1600 },
                "extensions": ["scene"],
                "magic": "41434D45",
                "stand_in": "[{type} {size}] {name}"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let file = std::fs::read_to_string(dir.path().join("mime_types.toml")).unwrap();
        assert!(file.contains("x-acme-scene") && file.contains("per_pixel"));

        // Updated: the same key again is a 200 that says it was not created.
        let req = Request::builder()
            .method("PUT")
            .uri("/api/mime")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "mime_type": "application/x-acme-scene", "text": true })
                    .to_string(),
            ))
            .unwrap();
        let resp = app(dir.path().join("config.toml"))
            .oneshot(req)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let written: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(written["created"], false);

        // Removed, then removing again is a 404.
        assert_eq!(
            delete_row(dir.path(), "application/x-acme-scene").await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            delete_row(dir.path(), "application/x-acme-scene").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn each_token_rule_shape_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        for (i, tokens) in [
            serde_json::json!({ "per_byte": 0.25 }),
            serde_json::json!({ "per_second": 32 }),
            serde_json::json!({ "fixed": 1000 }),
            serde_json::json!({ "per_pixel": 750 }),
        ]
        .into_iter()
        .enumerate()
        {
            let status = put_row(
                dir.path(),
                serde_json::json!({ "mime_type": format!("model/x-{i}"), "tokens": tokens }),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "rule {i}");
        }
    }

    #[tokio::test]
    async fn a_bad_type_a_bad_rule_and_a_bad_magic_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        // A type that is not `type/subtype`.
        assert_eq!(
            put_row(dir.path(), serde_json::json!({ "mime_type": "notamime" })).await,
            StatusCode::BAD_REQUEST
        );
        // A token rule that names none of the four.
        assert_eq!(
            put_row(
                dir.path(),
                serde_json::json!({ "mime_type": "model/x", "tokens": { "max": 5 } })
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        // `max` without `per_pixel`.
        assert_eq!(
            put_row(
                dir.path(),
                serde_json::json!({ "mime_type": "model/x", "tokens": { "per_byte": 0.1, "max": 5 } })
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        // A row whose `check` names a script that will not compile fails the
        // same validation `lev mime add` runs, so nothing is written.
        assert_eq!(
            put_row(
                dir.path(),
                serde_json::json!({ "mime_type": "model/x", "magic": "nothex" })
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        // A malformed type on delete is a 400 before the file is touched.
        assert_eq!(
            delete_row(dir.path(), "notamime").await,
            StatusCode::BAD_REQUEST
        );
    }
}
