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

/// Whether a discovery URL is safe to fetch.
///
/// HTTPS, or HTTP on loopback. Everything in the OAuth discovery chain carries
/// or leads to a bearer token, and `http://` to a remote host puts that token on
/// the wire in cleartext. The loopback exemption is for developing against a
/// local authorization server, where there is no network to intercept.
pub(crate) fn is_safe_discovery_url(url: &Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => matches!(
            url.host(),
            Some(url::Host::Domain("localhost"))
                | Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST))
                | Some(url::Host::Ipv6(std::net::Ipv6Addr::LOCALHOST))
        ),
        _ => false,
    }
}

/// Whether two URLs share an origin (scheme, host, and effective port).
///
/// Used to bind a server-supplied `resource_metadata` URL to the MCP server that
/// offered it. That URL arrives in a `WWW-Authenticate` header — entirely under
/// the remote server's control — and was previously fetched with no check at
/// all, which made every MCP connection an SSRF primitive pointed at whatever
/// the server named, including cloud metadata endpoints.
pub(crate) fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host() == b.host()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// The candidate authorization-server metadata URLs to try, in order.
///
/// RFC 8414 first, then the OpenID Connect discovery document as a fallback:
/// some issuers publish only the latter.
/// Infallible: the caller has already parsed the issuer (and reported a bad one),
/// and these are constant valid paths joined onto an existing base. Taking a
/// `&Url` rather than a `&str` is what removes the second, unreachable parse.
pub(crate) fn auth_server_metadata_urls(base: &Url) -> Vec<Url> {
    let rfc8414 = base
        .join("/.well-known/oauth-authorization-server")
        .expect("RFC 8414 path is always joinable");
    let openid = base
        .join("/.well-known/openid-configuration")
        .expect("OpenID configuration path is always joinable");
    vec![rfc8414, openid]
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
        let urls = auth_server_metadata_urls(&u("https://auth.example.com"));
        assert_eq!(
            urls[0].as_str(),
            "https://auth.example.com/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            urls[1].as_str(),
            "https://auth.example.com/.well-known/openid-configuration"
        );
    }

    // A bad issuer no longer reaches this function: it takes an already-parsed
    // `Url`, so the caller reports an unparseable issuer and this cannot be
    // handed one. `login_refuses_metadata_with_an_unparseable_issuer` in
    // `auth::tests` covers that path end to end.

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

    fn u(s: &str) -> Url {
        Url::parse(s).expect("test URL parses")
    }

    /// Everything in the discovery chain carries or leads to a bearer token, so
    /// plain HTTP to a remote host would put that token on the wire.
    #[test]
    fn discovery_requires_https_off_loopback() {
        assert!(is_safe_discovery_url(&u("https://auth.example.com/x")));
        assert!(!is_safe_discovery_url(&u("http://auth.example.com/x")));
        assert!(!is_safe_discovery_url(&u("ftp://auth.example.com/x")));
    }

    /// ...but a local authorization server has no network to intercept, and
    /// refusing it would break every local-dev setup (and this crate's own mock
    /// server tests).
    #[test]
    fn discovery_permits_http_on_loopback() {
        for url in [
            "http://localhost:8080/x",
            "http://127.0.0.1:8080/x",
            "http://[::1]:8080/x",
        ] {
            assert!(is_safe_discovery_url(&u(url)), "{url}");
        }
        // Not loopback, just similarly named.
        assert!(!is_safe_discovery_url(&u("http://localhost.evil.com/x")));
    }

    /// The `resource_metadata` hint comes from a header the remote server wrote.
    /// Binding it to that server's own origin is what stops a hostile MCP server
    /// from making us fetch an arbitrary URL from inside the user's network.
    #[test]
    fn same_origin_compares_scheme_host_and_port() {
        let mcp = u("https://mcp.example.com/mcp");
        assert!(same_origin(
            &u("https://mcp.example.com/.well-known/x"),
            &mcp
        ));
        // Default port is equivalent to the explicit one.
        assert!(same_origin(&u("https://mcp.example.com:443/x"), &mcp));

        assert!(!same_origin(&u("https://evil.example.com/x"), &mcp));
        assert!(!same_origin(&u("http://mcp.example.com/x"), &mcp));
        assert!(!same_origin(&u("https://mcp.example.com:8443/x"), &mcp));
        // A subdomain is a different origin.
        assert!(!same_origin(&u("https://a.mcp.example.com/x"), &mcp));
        // The classic near-miss.
        assert!(!same_origin(&u("https://mcp.example.com.evil.com/x"), &mcp));
        // Cloud metadata, the thing this ultimately protects.
        assert!(!same_origin(&u("http://169.254.169.254/latest/"), &mcp));
    }
}
