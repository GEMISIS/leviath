//! Exchanging a refresh token for a new one, over HTTP.
//!
//! Split from [`super::token`] so the single-flight logic there can be tested
//! exhaustively without a socket. That separation is what makes it possible to
//! prove the eight-concurrent-callers case, which is the one that matters:
//! rotation makes a double refresh terminal.

use async_trait::async_trait;

use super::token::{RefreshError, RefreshTransport, RefreshedTokens};

/// The real refresh, against the issuer.
pub struct HttpRefresh {
    client: reqwest::Client,
    token_url: String,
    client_id: String,
}

impl HttpRefresh {
    /// A refresher against the public ChatGPT issuer.
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            token_url: format!("{}/oauth/token", super::ISSUER),
            client_id: super::CLIENT_ID.to_string(),
        }
    }

    /// Point at a different token endpoint. Tests use this.
    #[must_use]
    pub fn with_token_url(mut self, url: String) -> Self {
        self.token_url = url;
        self
    }
}

/// Whether an error body says the grant is gone rather than merely unhappy.
///
/// RFC 6749 reports an unusable refresh token as `invalid_grant` without
/// preserving whether it expired, was revoked, or was already spent. The
/// subtypes are matched where the issuer still sends them, because they make a
/// far better message, and `invalid_grant` is the terminal catch-all.
fn is_terminal(status: u16, body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    if status == 400 || status == 401 {
        return body.contains("invalid_grant")
            || body.contains("refresh_token_expired")
            || body.contains("refresh_token_reused")
            || body.contains("refresh_token_invalidated")
            || body.contains("invalid_request");
    }
    false
}

/// The sentence to show for a terminal refusal.
fn terminal_message(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    if lower.contains("refresh_token_reused") {
        return "the ChatGPT refresh token was rejected as already used. This happens when \
                two processes refresh the same session at once. The session cannot be \
                recovered: run `lev auth login codex` to sign in again"
            .to_string();
    }
    if lower.contains("refresh_token_invalidated") {
        return "the ChatGPT session was revoked. Run `lev auth login codex` to sign in again"
            .to_string();
    }
    "the ChatGPT session has expired. Run `lev auth login codex` to sign in again".to_string()
}

#[async_trait]
impl RefreshTransport for HttpRefresh {
    async fn refresh(&self, refresh_token: &str) -> Result<RefreshedTokens, RefreshError> {
        // JSON, not the form encoding the authorization-code exchange uses.
        let body = serde_json::json!({
            "client_id": self.client_id,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        });

        let response = self
            .client
            .post(&self.token_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| RefreshError::Transient(format!("could not reach the issuer: {e}")))?;

        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();

        if !(200..300).contains(&status) {
            return Err(match is_terminal(status, &text) {
                true => RefreshError::Terminal(terminal_message(&text)),
                false => RefreshError::Transient(format!(
                    "the issuer refused the refresh (HTTP {status}): {text}"
                )),
            });
        }

        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            RefreshError::Transient(format!("the issuer's reply was not JSON: {e}"))
        })?;
        let string = |key: &str| {
            parsed
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let access_token = string("access_token").ok_or_else(|| {
            RefreshError::Transient("the issuer's reply carried no access token".to_string())
        })?;

        Ok(RefreshedTokens {
            access_token,
            refresh_token: string("refresh_token"),
            id_token: string("id_token"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_testkit::spawn_mock_server;

    fn refresher(url: &str) -> HttpRefresh {
        HttpRefresh::new(reqwest::Client::new()).with_token_url(url.to_string())
    }

    #[tokio::test]
    async fn a_rotated_pair_comes_back() {
        let url = spawn_mock_server(
            200,
            "OK",
            br#"{"access_token":"at-new","refresh_token":"rt-new","id_token":"id-new"}"#.to_vec(),
        )
        .await;
        let tokens = refresher(&url).refresh("rt-old").await.expect("refresh");
        assert_eq!(tokens.access_token, "at-new");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-new"));
        assert_eq!(tokens.id_token.as_deref(), Some("id-new"));
    }

    #[tokio::test]
    async fn a_reply_that_rotates_nothing_still_works() {
        let url = spawn_mock_server(200, "OK", br#"{"access_token":"at-new"}"#.to_vec()).await;
        let tokens = refresher(&url).refresh("rt-old").await.expect("refresh");
        assert_eq!(tokens.refresh_token, None);
        assert_eq!(tokens.id_token, None);
    }

    #[tokio::test]
    async fn a_reused_refresh_token_is_terminal_and_says_why() {
        // The message has to explain itself: this is unrecoverable, and the
        // cause (two processes refreshing at once) is not guessable.
        let url = spawn_mock_server(
            400,
            "Bad Request",
            br#"{"error":"refresh_token_reused"}"#.to_vec(),
        )
        .await;
        let err = refresher(&url).refresh("rt-old").await.unwrap_err();
        assert!(err.is_terminal());
        assert!(err.to_string().contains("already used"), "got {err}");
        assert!(
            err.to_string().contains("lev auth login codex"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn a_revoked_session_is_terminal() {
        let url = spawn_mock_server(
            400,
            "Bad Request",
            br#"{"error":"refresh_token_invalidated"}"#.to_vec(),
        )
        .await;
        let err = refresher(&url).refresh("rt-old").await.unwrap_err();
        assert!(err.is_terminal());
        assert!(err.to_string().contains("revoked"), "got {err}");
    }

    #[tokio::test]
    async fn a_bare_invalid_grant_is_terminal_with_the_generic_reason() {
        // RFC 6749 drops the subtype, so this is the common shape.
        let url =
            spawn_mock_server(400, "Bad Request", br#"{"error":"invalid_grant"}"#.to_vec()).await;
        let err = refresher(&url).refresh("rt-old").await.unwrap_err();
        assert!(err.is_terminal());
        assert!(err.to_string().contains("expired"), "got {err}");
    }

    #[tokio::test]
    async fn a_server_error_is_transient() {
        // The grant is presumably still good; retrying is the right move.
        let url = spawn_mock_server(503, "Service Unavailable", b"down".to_vec()).await;
        let err = refresher(&url).refresh("rt-old").await.unwrap_err();
        assert!(!err.is_terminal());
        assert!(err.to_string().contains("503"), "got {err}");
    }

    #[tokio::test]
    async fn a_401_that_is_not_about_the_grant_is_still_transient() {
        let url = spawn_mock_server(401, "Unauthorized", b"who are you".to_vec()).await;
        let err = refresher(&url).refresh("rt-old").await.unwrap_err();
        assert!(!err.is_terminal());
    }

    #[tokio::test]
    async fn an_unreachable_issuer_is_transient() {
        let err = refresher("http://127.0.0.1:1")
            .refresh("rt-old")
            .await
            .unwrap_err();
        assert!(!err.is_terminal());
        assert!(err.to_string().contains("could not reach"), "got {err}");
    }

    #[tokio::test]
    async fn a_reply_that_is_not_json_is_transient() {
        let url = spawn_mock_server(200, "OK", b"not json".to_vec()).await;
        let err = refresher(&url).refresh("rt-old").await.unwrap_err();
        assert!(!err.is_terminal());
        assert!(err.to_string().contains("not JSON"), "got {err}");
    }

    #[tokio::test]
    async fn a_reply_with_no_access_token_is_transient() {
        // Nothing to use, but nothing saying the grant is gone either.
        let url = spawn_mock_server(200, "OK", br#"{"id_token":"only-this"}"#.to_vec()).await;
        let err = refresher(&url).refresh("rt-old").await.unwrap_err();
        assert!(!err.is_terminal());
        assert!(err.to_string().contains("no access token"), "got {err}");
    }

    #[test]
    fn the_default_refresher_points_at_the_public_issuer() {
        let refresher = HttpRefresh::new(reqwest::Client::new());
        assert_eq!(refresher.token_url, "https://auth.openai.com/oauth/token");
        assert_eq!(refresher.client_id, super::super::CLIENT_ID);
    }

    #[test]
    fn only_the_grant_failures_are_terminal() {
        assert!(is_terminal(400, "invalid_grant"));
        assert!(is_terminal(401, "REFRESH_TOKEN_EXPIRED"));
        assert!(!is_terminal(400, "something else entirely"));
        assert!(!is_terminal(500, "invalid_grant"));
        assert!(!is_terminal(429, "slow down"));
    }
}
