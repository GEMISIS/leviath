//! Exchanging a refresh token for a new one, over HTTP.
//!
//! Split from [`super::token`] so the single-flight logic there can be tested
//! exhaustively without a socket. That separation is what makes it possible to
//! prove the eight-concurrent-callers case, which is the one that matters:
//! rotation makes a double refresh terminal.

use async_trait::async_trait;

use super::token::{RefreshError, RefreshTransport, RefreshedTokens};
use super::{OAuthProfile, TokenBody};

/// The real refresh, against the issuer.
pub struct HttpRefresh {
    client: reqwest::Client,
    token_url: String,
    profile: &'static OAuthProfile,
}

impl HttpRefresh {
    /// A refresher against `profile`'s issuer.
    pub fn new(client: reqwest::Client, profile: &'static OAuthProfile) -> Self {
        Self {
            client,
            token_url: profile.token_url(profile.issuer),
            profile,
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
fn terminal_message(profile: &OAuthProfile, body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    let account = profile.brand;
    let hint = profile.relogin_hint();
    if lower.contains("refresh_token_reused") {
        return format!(
            "the {account} refresh token was rejected as already used. This happens when \
             two processes refresh the same session at once. The session cannot be \
             recovered: {hint}"
        );
    }
    if lower.contains("refresh_token_invalidated") {
        return format!("the {account} session was revoked. {}", capitalised(&hint));
    }
    format!("the {account} session has expired. {}", capitalised(&hint))
}

/// `hint` with its first letter upper-cased, to start a sentence.
fn capitalised(hint: &str) -> String {
    let mut chars = hint.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[async_trait]
impl RefreshTransport for HttpRefresh {
    async fn refresh(&self, refresh_token: &str) -> Result<RefreshedTokens, RefreshError> {
        let pairs = [
            ("client_id", self.profile.client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        // Each issuer's own choice, and neither accepts the other: OpenAI's
        // wants JSON, and xAI's answers a JSON body with 415.
        let request = self.client.post(&self.token_url);
        let request = match self.profile.refresh_body {
            TokenBody::Json => request.json(
                &pairs
                    .iter()
                    .map(|(k, v)| (*k, *v))
                    .collect::<std::collections::BTreeMap<_, _>>(),
            ),
            TokenBody::Form => request.form(&pairs),
        };
        let response = request
            .send()
            .await
            .map_err(|e| RefreshError::Transient(format!("could not reach the issuer: {e}")))?;

        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();

        if !(200..300).contains(&status) {
            return Err(match is_terminal(status, &text) {
                true => RefreshError::Terminal(terminal_message(self.profile, &text)),
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
        HttpRefresh::new(reqwest::Client::new(), &crate::codex::PROFILE)
            .with_token_url(url.to_string())
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
        let refresher = HttpRefresh::new(reqwest::Client::new(), &crate::codex::PROFILE);
        assert_eq!(refresher.token_url, "https://auth.openai.com/oauth/token");
        let grok = HttpRefresh::new(reqwest::Client::new(), &crate::grok::PROFILE);
        assert_eq!(grok.token_url, "https://auth.x.ai/oauth2/token");
    }

    #[tokio::test]
    async fn each_issuer_gets_the_body_encoding_it_accepts() {
        let reply = br#"{"access_token":"at-new"}"#.to_vec();
        let (url, seen) = leviath_testkit::spawn_mock_recorder(200, "OK", reply.clone()).await;
        HttpRefresh::new(reqwest::Client::new(), &crate::grok::PROFILE)
            .with_token_url(url)
            .refresh("rt-old")
            .await
            .expect("refresh");
        let form = seen.lock().unwrap().join("\n");
        assert!(form.contains("application/x-www-form-urlencoded"), "{form}");
        assert!(form.contains("grant_type=refresh_token"), "{form}");
        assert!(form.contains("refresh_token=rt-old"), "{form}");

        let (url, seen) = leviath_testkit::spawn_mock_recorder(200, "OK", reply).await;
        refresher(&url).refresh("rt-old").await.expect("refresh");
        let json = seen.lock().unwrap().join("\n");
        assert!(json.contains("application/json"), "{json}");
        assert!(json.contains("\"grant_type\":\"refresh_token\""), "{json}");
    }

    #[tokio::test]
    async fn a_terminal_refusal_names_the_account_and_the_provider() {
        let url =
            spawn_mock_server(400, "Bad Request", br#"{"error":"invalid_grant"}"#.to_vec()).await;
        let err = HttpRefresh::new(reqwest::Client::new(), &crate::grok::PROFILE)
            .with_token_url(url)
            .refresh("rt-old")
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("Grok session has expired"), "{text}");
        assert!(text.contains("Run `lev auth login grok`"), "{text}");
    }

    #[test]
    fn an_empty_hint_capitalises_to_nothing() {
        assert_eq!(capitalised(""), "");
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
