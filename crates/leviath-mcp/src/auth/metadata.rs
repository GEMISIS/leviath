//! OAuth metadata discovery for MCP servers.
//!
//! The chain the MCP authorization spec prescribes:
//!
//! 1. an unauthenticated request to the MCP server returns `401` with a
//!    `WWW-Authenticate` header pointing at the protected-resource metadata;
//! 2. that document (RFC 9728) names the authorization server(s);
//! 3. the authorization server's metadata (RFC 8414) names the authorize,
//!    token and registration endpoints.
//!
//! Each step has a well-known fallback so a server that omits a hint still
//! works.

use reqwest::Url;
use serde::Deserialize;

/// Protected-resource metadata (RFC 9728).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProtectedResourceMetadata {
    /// The canonical resource identifier, bound into tokens (RFC 8707).
    #[serde(default)]
    pub(crate) resource: String,
    /// Authorization servers that can issue tokens for this resource.
    #[serde(default)]
    pub(crate) authorization_servers: Vec<String>,
}

/// Authorization-server metadata (RFC 8414 / OpenID Discovery).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AuthServerMetadata {
    pub(crate) issuer: String,
    pub(crate) authorization_endpoint: String,
    pub(crate) token_endpoint: String,
    #[serde(default)]
    pub(crate) registration_endpoint: Option<String>,
    #[serde(default)]
    pub(crate) scopes_supported: Vec<String>,
}

/// Extract the `resource_metadata="…"` URL from a `WWW-Authenticate` header.
///
/// Returns `None` when the header is absent or carries no such parameter, in
/// which case the caller falls back to the well-known path.
#[expect(
    clippy::string_slice,
    reason = "`start` is a `find` hit plus the length of the ASCII needle it matched, and `end` is \
              a `find` hit or the length — all char boundaries"
)]
pub(crate) fn resource_metadata_url(www_authenticate: Option<&str>) -> Option<String> {
    let header = www_authenticate?;
    // The parameter looks like: Bearer resource_metadata="https://…", …
    let start = header.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = &header[start..];
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let end = rest.find('"').unwrap_or(rest.len());
    let url = rest[..end].trim();
    if url.is_empty() {
        None
    } else {
        Some(url.to_string())
    }
}

/// The well-known protected-resource metadata URL for an MCP endpoint.
///
/// Per RFC 9728 the document lives at the *origin*, so any path on the MCP URL
/// is dropped.
pub(crate) fn well_known_resource_url(mcp_url: &Url) -> Url {
    // Joining a constant, valid path onto an already-parsed URL cannot fail.
    mcp_url
        .join("/.well-known/oauth-protected-resource")
        .expect("well-known path is always joinable")
}

/// The candidate authorization-server metadata URLs to try, in order.
///
/// RFC 8414 first, then the OpenID Connect discovery document as a fallback:
/// some issuers publish only the latter.
pub(crate) fn auth_server_metadata_urls(issuer: &str) -> anyhow::Result<Vec<Url>> {
    let base = Url::parse(issuer)
        .map_err(|e| anyhow::anyhow!("Invalid authorization server issuer '{}': {}", issuer, e))?;
    // These joins are constant valid paths onto an already-parsed base, so
    // only the `parse` above can fail.
    let rfc8414 = base
        .join("/.well-known/oauth-authorization-server")
        .expect("RFC 8414 path is always joinable");
    let openid = base
        .join("/.well-known/openid-configuration")
        .expect("OpenID configuration path is always joinable");
    Ok(vec![rfc8414, openid])
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── WWW-Authenticate parsing ─────────────────────────────────────────

    #[test]
    fn extracts_a_quoted_resource_metadata_url() {
        let header = r#"Bearer resource_metadata="https://s.example.com/.well-known/x", error="x""#;
        assert_eq!(
            resource_metadata_url(Some(header)).as_deref(),
            Some("https://s.example.com/.well-known/x")
        );
    }

    #[test]
    fn extracts_an_unquoted_resource_metadata_url() {
        let header = "Bearer resource_metadata=https://s.example.com/meta";
        assert_eq!(
            resource_metadata_url(Some(header)).as_deref(),
            Some("https://s.example.com/meta")
        );
    }

    #[test]
    fn absent_header_yields_none() {
        assert_eq!(resource_metadata_url(None), None);
    }

    #[test]
    fn header_without_the_parameter_yields_none() {
        assert_eq!(
            resource_metadata_url(Some("Bearer error=\"invalid\"")),
            None
        );
    }

    #[test]
    fn empty_parameter_value_yields_none() {
        assert_eq!(
            resource_metadata_url(Some(r#"Bearer resource_metadata="""#)),
            None
        );
    }

    // ─── well-known derivation ────────────────────────────────────────────

    #[test]
    fn well_known_resource_drops_the_mcp_path() {
        let url = Url::parse("https://mcp.example.com/some/mcp").unwrap();
        assert_eq!(
            well_known_resource_url(&url).as_str(),
            "https://mcp.example.com/.well-known/oauth-protected-resource"
        );
    }

    #[test]
    fn auth_server_urls_offer_rfc8414_then_openid() {
        let urls = auth_server_metadata_urls("https://auth.example.com").unwrap();
        assert_eq!(
            urls[0].as_str(),
            "https://auth.example.com/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            urls[1].as_str(),
            "https://auth.example.com/.well-known/openid-configuration"
        );
    }

    #[test]
    fn auth_server_urls_reject_a_bad_issuer() {
        assert!(auth_server_metadata_urls("not a url").is_err());
    }

    // ─── deserialization ──────────────────────────────────────────────────

    #[test]
    fn protected_resource_metadata_parses() {
        let json = r#"{
            "resource": "https://mcp.example.com/mcp",
            "authorization_servers": ["https://auth.example.com"],
            "scopes_supported": ["openid"]
        }"#;
        let meta: ProtectedResourceMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.resource, "https://mcp.example.com/mcp");
        assert_eq!(meta.authorization_servers, vec!["https://auth.example.com"]);
    }

    #[test]
    fn auth_server_metadata_parses_with_optional_registration() {
        let json = r#"{
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token",
            "registration_endpoint": "https://auth.example.com/register",
            "scopes_supported": ["openid", "profile"]
        }"#;
        let meta: AuthServerMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.issuer, "https://auth.example.com");
        assert_eq!(
            meta.registration_endpoint.as_deref(),
            Some("https://auth.example.com/register")
        );
        assert_eq!(meta.scopes_supported.len(), 2);
    }

    #[test]
    fn auth_server_metadata_registration_is_optional() {
        let json = r#"{
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token"
        }"#;
        let meta: AuthServerMetadata = serde_json::from_str(json).unwrap();
        assert!(meta.registration_endpoint.is_none());
        assert!(meta.scopes_supported.is_empty());
    }
}
