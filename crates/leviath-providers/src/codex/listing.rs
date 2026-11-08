//! The route's own model listing, for the account that is signed in.
//!
//! `GET {base}/models?client_version=...` answers with the models this account
//! can actually use, already narrowed by plan: measured on a Plus account, it
//! omitted `gpt-5.5` and `gpt-5.3-codex-spark`, and the route refused exactly
//! those two with `model_not_found`. It also reports each model's window. So
//! when it answers, it is better evidence than [`super::catalog::CATALOG`],
//! which stays as the offline fallback.

use serde::Deserialize;

/// The Codex CLI version the listing is asked for.
///
/// The route filters the list by the client's version (each model carries a
/// `minimal_client_version`), so a stale value silently hides new models.
pub(crate) const CLIENT_VERSION: &str = "0.162.1";

/// One model the route lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    /// The id a request names.
    pub id: String,
    /// The route's display name.
    pub display: String,
    /// `max_context_window`, when the route reports one.
    pub context: Option<usize>,
    /// Served but not meant to be picked (`visibility: "hide"`).
    pub hidden: bool,
}

#[derive(Deserialize)]
struct Body {
    models: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    max_context_window: Option<usize>,
    #[serde(default)]
    visibility: Option<String>,
}

/// The models in a `/models` body, or `None` when it is not that shape or
/// lists nothing (an empty answer is "cannot say", not "serves nothing").
pub(crate) fn parse(body: &str) -> Option<Vec<Listed>> {
    let parsed: Body = serde_json::from_str(body).ok()?;
    let listed: Vec<Listed> = parsed
        .models
        .into_iter()
        .map(|m| Listed {
            display: m.display_name.unwrap_or_else(|| m.slug.clone()),
            id: m.slug,
            context: m.max_context_window.filter(|c| *c > 0),
            hidden: m.visibility.as_deref() == Some("hide"),
        })
        .collect();
    (!listed.is_empty()).then_some(listed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_is_read_with_its_windows_and_hidden_rows() {
        let body = r#"{"models":[
            {"slug":"gpt-6.1-sol","display_name":"GPT-6.1-Sol","max_context_window":872000,"visibility":"list"},
            {"slug":"gpt-reserve","visibility":"hide"},
            {"slug":"zero","max_context_window":0}
        ]}"#;
        let listed = parse(body).expect("a listing");
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].id, "gpt-6.1-sol");
        assert_eq!(listed[0].display, "GPT-6.1-Sol");
        assert_eq!(listed[0].context, Some(872_000));
        assert!(!listed[0].hidden);
        assert!(listed[1].hidden);
        assert_eq!(listed[1].display, "gpt-reserve", "falls back to the id");
        assert_eq!(listed[2].context, None, "a zero window is no window");
    }

    #[test]
    fn a_body_that_is_not_a_listing_says_nothing() {
        assert!(parse("not json").is_none());
        assert!(parse(r#"{"models":[]}"#).is_none());
        assert!(parse(r#"{"other":1}"#).is_none());
    }
}
