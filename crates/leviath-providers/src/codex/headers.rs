//! The headers this route insists on, and what to say when it refuses.

use super::token::Credentials;

/// Build the header set for one inference request.
///
/// `originator` and the `User-Agent` derived from it are a pair: the backend
/// has been observed to whitelist the former, and a User-Agent that contradicts
/// it is exactly the mismatch a fingerprinting edge looks for. Deriving one
/// from the other keeps them from drifting apart in config.
pub fn inference(
    creds: &Credentials,
    originator: &str,
    user_agent: &str,
) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("Authorization", format!("Bearer {}", creds.access_token)),
        ("originator", originator.to_string()),
        ("User-Agent", user_agent.to_string()),
        ("OpenAI-Beta", "responses=experimental".to_string()),
        ("Accept", "text/event-stream".to_string()),
        ("Content-Type", "application/json".to_string()),
    ];
    if let Some(account) = &creds.account_id {
        // Sent in the casing the first-party client uses. HTTP header names are
        // case-insensitive, but an edge that fingerprints clients is not
        // obliged to be, and matching costs nothing.
        headers.push(("ChatGPT-Account-Id", account.clone()));
    }
    headers
}

/// The default `User-Agent` for an originator, in the shape the route expects.
pub fn user_agent_for(originator: &str, version: &str) -> String {
    format!(
        "{originator}/{version} ({} {}; {})",
        std::env::consts::OS,
        std::env::consts::FAMILY,
        std::env::consts::ARCH,
    )
}

/// What to tell a person when the route answers 403.
///
/// The stock "check the account's plan and model permissions" remedy sends them
/// somewhere useless. A 403 here is almost always the client identity, so the
/// message names what was actually sent: without that, the one setting that
/// could be wrong is invisible.
pub fn forbidden_remedy(originator: &str, user_agent: &str, body: &str) -> String {
    let excerpt: String = body.chars().take(400).collect();
    format!(
        "chatgpt.com refused the request (403). This route only accepts clients whose \
         `originator` it recognises. Leviath sent `originator: {originator}` with \
         `User-Agent: {user_agent}`. If you set `[providers] codex_originator` yourself, \
         remove it to go back to the default. If you did not, the backend may have \
         tightened which clients it accepts, or a proxy or VPN on this network may be \
         challenging the request. Response body: {excerpt}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(account: Option<&str>) -> Credentials {
        Credentials {
            access_token: "at-secret".to_string(),
            account_id: account.map(str::to_string),
        }
    }

    /// Look one header up by name.
    fn get<'a>(headers: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn the_bearer_and_the_stream_headers_are_always_present() {
        let headers = inference(&creds(None), "leviath", "leviath/0.1");
        assert_eq!(get(&headers, "Authorization"), Some("Bearer at-secret"));
        assert_eq!(get(&headers, "Accept"), Some("text/event-stream"));
        assert_eq!(get(&headers, "OpenAI-Beta"), Some("responses=experimental"));
        assert_eq!(get(&headers, "Content-Type"), Some("application/json"));
    }

    #[test]
    fn the_account_header_is_sent_when_the_grant_names_one() {
        let headers = inference(&creds(Some("acct-1")), "leviath", "leviath/0.1");
        assert_eq!(get(&headers, "ChatGPT-Account-Id"), Some("acct-1"));
    }

    #[test]
    fn a_grant_with_no_account_sends_no_account_header() {
        // Rather than an empty one, which reads as "this workspace" and is not
        // the same thing as "unspecified".
        let headers = inference(&creds(None), "leviath", "leviath/0.1");
        assert!(get(&headers, "ChatGPT-Account-Id").is_none());
    }

    #[test]
    fn the_originator_and_user_agent_are_both_carried() {
        let headers = inference(&creds(None), "Codex Leviath", "Codex Leviath/1.2");
        assert_eq!(get(&headers, "originator"), Some("Codex Leviath"));
        assert_eq!(get(&headers, "User-Agent"), Some("Codex Leviath/1.2"));
    }

    #[test]
    fn the_default_user_agent_names_the_originator_and_the_version() {
        let ua = user_agent_for("leviath", "0.5.6");
        assert!(ua.starts_with("leviath/0.5.6 ("), "got {ua}");
        assert!(ua.contains(std::env::consts::ARCH), "got {ua}");
    }

    #[test]
    fn the_forbidden_remedy_names_what_was_actually_sent() {
        // The one fact that makes this debuggable. Without it the setting that
        // could be wrong is invisible to the person reading the error.
        let message = forbidden_remedy("leviath", "leviath/0.5.6", "{\"detail\":\"nope\"}");
        assert!(message.contains("originator: leviath"), "{message}");
        assert!(message.contains("User-Agent: leviath/0.5.6"), "{message}");
        assert!(message.contains("codex_originator"), "{message}");
        assert!(message.contains("nope"), "{message}");
    }

    #[test]
    fn the_forbidden_remedy_bounds_the_body_it_quotes() {
        // A route that answers 403 with an HTML challenge page would otherwise
        // put the whole page in the log line.
        let message = forbidden_remedy("leviath", "ua", &"x".repeat(10_000));
        assert!(message.len() < 1_200, "message was {} chars", message.len());
    }

    #[test]
    fn a_multibyte_body_is_truncated_on_a_character_boundary() {
        // `chars().take()` rather than a byte slice: the latter panics here.
        let message = forbidden_remedy("leviath", "ua", &"日".repeat(10_000));
        assert!(message.contains('日'));
    }
}
