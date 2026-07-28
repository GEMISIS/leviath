//! Outbound-request policy: which URLs an agent-driven fetch may reach.
//!
//! Agents fetch URLs the model chose, and the model chose them from context an
//! attacker can influence — search results, a page fetched a moment ago, a task
//! description pasted from an issue. Handing such a URL straight to an HTTP
//! client turns the agent into a confused deputy sitting *inside* the user's
//! network: `http://169.254.169.254/latest/meta-data/iam/security-credentials/`
//! returns cloud credentials, `http://127.0.0.1:3000/api/agents` is the user's
//! own Leviath API, and `http://192.168.1.1/` is their router.
//!
//! [`check_url`] is the gate. It runs before the request and again on every
//! redirect hop, because a public URL that answers `302 Location:
//! http://169.254.169.254/` reaches exactly the same place.
//!
//! ## What this does not cover
//!
//! A hostname is resolved here and then resolved *again* by the HTTP client when
//! it connects. A DNS entry with a very short TTL that answers publicly the first
//! time and privately the second slips through that window — classic DNS
//! rebinding. Closing it needs a custom connector that dials the exact address
//! this module approved, which the shared blocking client cannot express today.
//! The check still stops every direct attempt (literal IPs, hostnames that
//! resolve privately, and redirects), which is the whole of the reachable
//! surface for a model that is picking URLs rather than running an attack.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

/// The floor every outbound HTTP client in the workspace gets.
///
/// Two clients were built with a bare `reqwest::Client::new()` and therefore had
/// **no timeouts at all**: webhook delivery (so an endpoint that accepts a
/// connection and never answers hung that delivery forever) and the package
/// registry. `Client::new()` is an easy default to reach for and a bad one for
/// anything talking to a host we do not control.
///
/// Values are deliberately generous — this is a floor to stop a hang, not a
/// performance budget. A caller with a real reason for different numbers builds
/// its own client and says why, as the provider and MCP transports do.
#[derive(Debug, Clone, Copy)]
pub struct ClientTimeouts {
    /// Cap on establishing the TCP+TLS connection.
    pub connect: Duration,
    /// Cap on the whole request/response.
    pub total: Duration,
}

impl Default for ClientTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            total: Duration::from_secs(60),
        }
    }
}

/// A `reqwest` client builder carrying the shared timeout floor and a redirect
/// cap.
///
/// The redirect cap matters independently of timeouts: reqwest follows up to ten
/// hops by default, and every hop is a fresh destination that the caller's
/// original URL check never saw.
pub fn client_builder(timeouts: ClientTimeouts) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(timeouts.connect)
        .timeout(timeouts.total)
        .redirect(reqwest::redirect::Policy::limited(5))
}

/// Why a URL was refused. Rendered into the tool result the model sees, so it
/// says what to do differently rather than just failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlRejection {
    /// The scheme is not `http` or `https`.
    Scheme(String),
    /// The URL has no host (e.g. `http:///path`).
    NoHost,
    /// The host did not resolve to any address.
    Unresolvable(String),
    /// The host resolved to an address outside the public internet.
    PrivateAddress {
        /// The host as written in the URL.
        host: String,
        /// The address it resolved to (or was written as).
        addr: IpAddr,
    },
}

impl std::fmt::Display for UrlRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scheme(s) => write!(
                f,
                "scheme '{s}' is not fetchable — only http and https are allowed"
            ),
            Self::NoHost => write!(f, "URL has no host"),
            Self::Unresolvable(h) => write!(f, "host '{h}' did not resolve"),
            Self::PrivateAddress { host, addr } => write!(
                f,
                "'{host}' resolves to {addr}, which is on a loopback, private, or \
                 link-local network. Fetching it from an agent would reach the \
                 user's own machine or LAN — including cloud metadata services \
                 that hand out credentials. Set `[security] allow_local_network = \
                 true` if this agent is genuinely meant to talk to a local service."
            ),
        }
    }
}

/// Whether `addr` is outside the public internet, and so off-limits to a fetch
/// whose URL an agent chose.
///
/// Covers loopback, RFC 1918 private, link-local (which includes the
/// `169.254.169.254` cloud metadata endpoint), CGNAT, unspecified, multicast,
/// broadcast, and documentation ranges — plus the IPv6 equivalents. IPv4-mapped
/// IPv6 addresses are unwrapped first, so `::ffff:127.0.0.1` cannot be used to
/// smuggle a loopback address past a v6 check.
pub fn is_restricted_addr(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_restricted_v4(v4),
        // `::ffff:a.b.c.d` is the same host as `a.b.c.d`; classify it as v4 so
        // the v4 rules (which are the detailed ones) actually apply.
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_restricted_v4(v4),
            None => is_restricted_v6(v6),
        },
    }
}

fn is_restricted_v4(a: Ipv4Addr) -> bool {
    let [b0, b1, ..] = a.octets();
    a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_unspecified()
        || a.is_multicast()
        || a.is_broadcast()
        || a.is_documentation()
        // 100.64.0.0/10 — carrier-grade NAT. `Ipv4Addr::is_shared` is still
        // unstable, so the range is spelled out.
        || (b0 == 100 && (64..128).contains(&b1))
        // 192.0.0.0/24 — IETF protocol assignments.
        || (b0 == 192 && b1 == 0 && a.octets()[2] == 0)
        // 240.0.0.0/4 — reserved.
        || b0 >= 240
}

fn is_restricted_v6(a: Ipv6Addr) -> bool {
    let seg0 = a.segments()[0];
    a.is_loopback()
        || a.is_unspecified()
        || a.is_multicast()
        // fc00::/7 — unique local. `is_unique_local` is unstable.
        || (seg0 & 0xfe00) == 0xfc00
        // fe80::/10 — link-local unicast. `is_unicast_link_local` is unstable.
        || (seg0 & 0xffc0) == 0xfe80
}

/// Check one URL against the outbound policy.
///
/// `allow_local` comes from `[security] allow_local_network`; when set, only the
/// scheme check applies, so an agent pointed at a self-hosted model or a service
/// on localhost still works. It is deliberately a machine-wide switch rather
/// than something a blueprint can assert about itself.
///
/// A literal IP in the URL is checked directly; a hostname is resolved and
/// *every* address it resolves to must pass, so a name with both a public and a
/// private A record is refused rather than raced.
pub fn check_url(url: &url::Url, allow_local: bool) -> Result<(), UrlRejection> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(UrlRejection::Scheme(other.to_string())),
    }
    if allow_local {
        return Ok(());
    }
    // `url::Host`, not `host_str()`: the latter keeps the brackets on an IPv6
    // literal (`[::1]`), which then fails to parse as an address *and* fails to
    // resolve — refusing the URL, but as "unresolvable" rather than "loopback",
    // and only by accident. `Host` hands back the address already parsed.
    let host = url.host().ok_or(UrlRejection::NoHost)?;

    // A literal address needs no DNS, and must not get it: going through the
    // resolver for something already unambiguous only adds a failure mode.
    let (host_str, literal) = match host {
        url::Host::Ipv4(v4) => (v4.to_string(), Some(IpAddr::V4(v4))),
        url::Host::Ipv6(v6) => (v6.to_string(), Some(IpAddr::V6(v6))),
        url::Host::Domain(d) => (d.to_string(), None),
    };
    if let Some(addr) = literal {
        return match is_restricted_addr(addr) {
            true => Err(UrlRejection::PrivateAddress {
                host: host_str,
                addr,
            }),
            false => Ok(()),
        };
    }

    let host = host_str.as_str();
    let port = url.port_or_known_default().unwrap_or(80);
    let resolved: Vec<IpAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|_| UrlRejection::Unresolvable(host.to_string()))?
        .map(|sa| sa.ip())
        .collect();
    if resolved.is_empty() {
        return Err(UrlRejection::Unresolvable(host.to_string()));
    }
    // Every address must pass. Rejecting on *any* restricted answer means a
    // hostname that resolves to both 93.184.216.34 and 127.0.0.1 is refused
    // rather than depending on which one the client happens to dial.
    for addr in resolved {
        if is_restricted_addr(addr) {
            return Err(UrlRejection::PrivateAddress {
                host: host.to_string(),
                addr,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> url::Url {
        url::Url::parse(s).expect("test URL parses")
    }

    #[test]
    fn rejects_non_http_schemes() {
        for s in ["file:///etc/passwd", "ftp://example.com/x", "gopher://x/"] {
            let err = check_url(&u(s), false).unwrap_err();
            assert!(matches!(err, UrlRejection::Scheme(_)), "{s} → {err:?}");
        }
    }

    /// The headline case: an agent talked into fetching the cloud metadata
    /// endpoint gets credentials for the whole instance.
    #[test]
    fn rejects_cloud_metadata_endpoint() {
        let err = check_url(&u("http://169.254.169.254/latest/meta-data/"), false).unwrap_err();
        assert!(matches!(err, UrlRejection::PrivateAddress { .. }));
    }

    /// Asserting on `PrivateAddress` specifically, not just `is_err()`: an IPv6
    /// literal used to be refused as *unresolvable* (the brackets from
    /// `host_str()` parsed as neither an address nor a name), which passed an
    /// `is_err()` check while the address rules never ran at all.
    #[test]
    fn rejects_loopback_and_private_literals() {
        for s in [
            "http://127.0.0.1:3000/api/agents",
            "http://0.0.0.0/",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://100.64.0.1/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
        ] {
            let err = check_url(&u(s), false).unwrap_err();
            assert!(
                matches!(err, UrlRejection::PrivateAddress { .. }),
                "{s} → {err:?}"
            );
        }
    }

    /// `::ffff:127.0.0.1` is loopback wearing a v6 costume.
    #[test]
    fn rejects_ipv4_mapped_loopback() {
        let err = check_url(&u("http://[::ffff:127.0.0.1]/"), false).unwrap_err();
        assert!(
            matches!(err, UrlRejection::PrivateAddress { .. }),
            "{err:?}"
        );
        assert!(is_restricted_addr("::ffff:10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn allows_public_literals() {
        for s in ["http://93.184.216.34/", "https://[2606:2800:220:1::]/"] {
            assert!(check_url(&u(s), false).is_ok(), "{s} should be allowed");
        }
    }

    /// The opt-out exists for people running a local model or a local service.
    #[test]
    fn allow_local_waives_the_address_check_but_not_the_scheme() {
        assert!(check_url(&u("http://127.0.0.1:11434/api/tags"), true).is_ok());
        assert!(check_url(&u("file:///etc/passwd"), true).is_err());
    }

    #[test]
    fn unresolvable_host_is_reported_as_such() {
        let err = check_url(&u("http://this-host-does-not-exist.invalid/"), false).unwrap_err();
        assert!(matches!(err, UrlRejection::Unresolvable(_)), "{err:?}");
    }

    /// `localhost` is a *name*, not a literal, so it exercises the resolver path
    /// rather than the parse-as-IP shortcut.
    #[test]
    fn rejects_localhost_by_name() {
        let err = check_url(&u("http://localhost:8080/"), false).unwrap_err();
        assert!(
            matches!(err, UrlRejection::PrivateAddress { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn rejection_messages_name_the_fix() {
        let err = check_url(&u("http://127.0.0.1/"), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("allow_local_network"), "{msg}");
        assert!(msg.contains("127.0.0.1"), "{msg}");

        assert!(UrlRejection::NoHost.to_string().contains("no host"));
        assert!(
            UrlRejection::Unresolvable("h".into())
                .to_string()
                .contains("did not resolve")
        );
        assert!(
            UrlRejection::Scheme("ftp".into())
                .to_string()
                .contains("only http and https")
        );
    }

    #[test]
    fn restricted_classification_covers_reserved_ranges() {
        for s in [
            "192.0.0.1", // IETF protocol assignments
            "240.0.0.1", // reserved
            "255.255.255.255",
            "224.0.0.1", // multicast
            "192.0.2.1", // documentation
        ] {
            assert!(is_restricted_addr(s.parse().unwrap()), "{s}");
        }
        assert!(!is_restricted_addr("8.8.8.8".parse().unwrap()));
        assert!(is_restricted_addr("ff02::1".parse().unwrap()));
        assert!(!is_restricted_addr("2001:4860:4860::8888".parse().unwrap()));
    }
}
