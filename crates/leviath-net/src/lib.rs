//! Outbound-request policy: which URLs an agent-driven fetch may reach.
//!
//! Agents fetch URLs the model chose, and the model chose them from context an
//! attacker can influence - search results, a page fetched a moment ago, a task
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
//! time and privately the second slips through that window - classic DNS
//! rebinding. Closing it needs a custom connector that dials the exact address
//! this module approved, which the shared blocking client cannot express today.
//! The check still stops every direct attempt (literal IPs, hostnames that
//! resolve privately, and redirects), which is the whole of the reachable
//! surface for a model that is picking URLs rather than running an attack.

pub mod read_caps;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

/// The floor every outbound HTTP client in the workspace gets.
///
/// A bare `reqwest::Client::new()` carries **no timeouts at all**, so an
/// endpoint that accepts a connection and never answers holds a webhook
/// delivery or a registry fetch open forever. It is an easy default to reach
/// for and a bad one for anything talking to a host we do not control.
///
/// Values are deliberately generous - this is a floor to stop a hang, not a
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
pub(crate) fn client_builder(timeouts: ClientTimeouts) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(timeouts.connect)
        .timeout(timeouts.total)
        .redirect(reqwest::redirect::Policy::limited(5))
}

/// A client with the shared floor applied, built.
///
/// `.expect`, not a fallback to `Client::new()`: `build()` fails only on TLS
/// backend initialization, and falling back would silently hand back exactly the
/// timeout-less client this exists to replace. It also matches what the script
/// host's shared client already does, and a fallback closure that never runs is
/// a permanently uncovered region.
pub fn client(timeouts: ClientTimeouts) -> reqwest::Client {
    client_builder(timeouts)
        .build()
        .expect("building an HTTP client fails only on TLS backend init")
}

/// Whether a redirect hop may be followed, and why not if it may not.
///
/// Split out of the policy closure so it can be tested without standing up a
/// redirecting server: the closure is then only the reqwest plumbing.
fn redirect_decision(
    previous_hops: usize,
    url: &url::Url,
    allow_local: bool,
) -> Result<(), String> {
    if previous_hops >= 5 {
        return Err("too many redirects".to_string());
    }
    check_url(url, allow_local).map_err(|e| format!("refused to follow redirect: {e}"))
}

/// A client whose redirects are policed by [`check_url`], so a permitted first
/// hop cannot be turned into a request somewhere else.
///
/// The SSRF check on the URL a caller passes is worth little on its own: a
/// cooperating server answers `302` and the default policy follows it to
/// `169.254.169.254` with the original headers attached. Every redirect is
/// re-checked here, and the chain is capped at five hops.
///
/// `allow_local` is threaded through to the same check, so a caller that
/// legitimately talks to `localhost` (an Ollama endpoint) does not have to
/// choose between that and having a redirect policy at all.
pub fn checked_client(timeouts: ClientTimeouts, allow_local: bool) -> reqwest::Client {
    client_builder(timeouts)
        .redirect(reqwest::redirect::Policy::custom(
            move |attempt| match redirect_decision(
                attempt.previous().len(),
                attempt.url(),
                allow_local,
            ) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            },
        ))
        .build()
        .expect("building an HTTP client fails only on TLS backend init")
}

/// Why a URL was refused. Rendered into the tool result the model sees, so it
/// says what to do differently rather than just failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlRejection {
    /// The scheme is not `http` or `https`.
    Scheme(String),
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

impl UrlRejection {
    /// A stable short name for this refusal.
    ///
    /// Exists so a test can `assert_eq!(err.kind(), "private_address")` rather
    /// than `assert!(matches!(err, ...))` - the `matches!` non-matching arm is a
    /// region only a *failing* assertion ever reaches, which reads as uncovered
    /// under the workspace's 100% gate. Useful in its own right for structured
    /// logging, where the kind is the field worth indexing on.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Scheme(_) => "scheme",
            Self::Unresolvable(_) => "unresolvable",
            Self::PrivateAddress { .. } => "private_address",
        }
    }
}

impl std::fmt::Display for UrlRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scheme(s) => write!(
                f,
                "scheme '{s}' is not fetchable - only http and https are allowed"
            ),
            Self::Unresolvable(h) => write!(f, "host '{h}' did not resolve"),
            Self::PrivateAddress { host, addr } => write!(
                f,
                "'{host}' resolves to {addr}, which is on a loopback, private, or \
                 link-local network. Fetching it from an agent would reach the \
                 user's own machine or LAN - including cloud metadata services \
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
/// broadcast, and documentation ranges - plus the IPv6 equivalents. IPv4-mapped
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
        // 100.64.0.0/10 - carrier-grade NAT. `Ipv4Addr::is_shared` is still
        // unstable, so the range is spelled out.
        || (b0 == 100 && (64..128).contains(&b1))
        // 192.0.0.0/24 - IETF protocol assignments.
        || (b0 == 192 && b1 == 0 && a.octets()[2] == 0)
        // 240.0.0.0/4 - reserved.
        || b0 >= 240
}

fn is_restricted_v6(a: Ipv6Addr) -> bool {
    let seg0 = a.segments()[0];
    a.is_loopback()
        || a.is_unspecified()
        || a.is_multicast()
        // fc00::/7 - unique local. `is_unique_local` is unstable.
        || (seg0 & 0xfe00) == 0xfc00
        // fe80::/10 - link-local unicast. `is_unicast_link_local` is unstable.
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
    check_url_with(url, allow_local, resolve_host).inspect_err(|rejection| {
        // The host only: a URL can carry userinfo or a signed query, and a
        // refusal is not a reason to write either into the log.
        tracing::debug!(host = url.host_str().unwrap_or(""), %rejection, "outbound URL refused");
    })
}

/// The real resolver: every address `host:port` maps to.
fn resolve_host(host: &str, port: u16) -> Option<Vec<IpAddr>> {
    (host, port)
        .to_socket_addrs()
        .ok()
        .map(|addrs| addrs.map(|sa| sa.ip()).collect())
}

/// Core of [`check_url`] with name resolution injected.
///
/// A `fn` pointer, not `impl Fn`, so there is a single monomorphization -
/// otherwise each caller's closure type gets its own coverage report and every
/// arm reads as partially covered. The seam exists because the interesting cases
/// (a name resolving to nothing, a name resolving to a public address, a name
/// resolving to *both* public and private) cannot be produced from real DNS in a
/// test without being slow, flaky, or dependent on someone else's zone file.
pub(crate) fn check_url_with(
    url: &url::Url,
    allow_local: bool,
    resolve: fn(&str, u16) -> Option<Vec<IpAddr>>,
) -> Result<(), UrlRejection> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(UrlRejection::Scheme(other.to_string())),
    }
    if allow_local {
        return Ok(());
    }
    // `url::Host`, not `host_str()`: the latter keeps the brackets on an IPv6
    // literal (`[::1]`), which then fails to parse as an address *and* fails to
    // resolve - refusing the URL, but as "unresolvable" rather than "loopback",
    // and only by accident. `Host` hands back the address already parsed.
    // `.expect`, not a `NoHost` variant: the scheme check above has already
    // restricted us to http/https, and the URL Standard requires a host for
    // those "special" schemes - `Url::parse("http://")` fails with "empty host"
    // rather than producing a hostless URL. A branch for the impossible case
    // would be a permanently-uncovered one.
    let host = url
        .host()
        .expect("http/https URLs always have a host per the URL Standard");

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
    // A resolver error and an empty answer are the same thing to a caller -
    // there is no address to check - so they collapse to one branch rather than
    // two, one of which would be near-impossible to reach.
    let resolved = resolve(host, port).filter(|addrs: &Vec<IpAddr>| !addrs.is_empty());
    let Some(resolved) = resolved else {
        return Err(UrlRejection::Unresolvable(host.to_string()));
    };
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

    /// Stand up a server that answers the first `hops` requests with a redirect
    /// to `target` and then `204`s, closing once `hops + 1` connections have
    /// been served so the task ends with the test.
    #[cfg(test)]
    async fn redirecting_server(target: String, hops: usize) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for i in 0..=hops {
                // `.expect`, not a fallible arm: an accept on a listener this
                // test owns does not fail, and the arm would be a region no
                // test can drive.
                let (mut sock, _) = listener.accept().await.expect("accept");
                // Read the request first: writing before the client has finished
                // sending confuses the connection and surfaces as a transport
                // error rather than the redirect under test.
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = match i < hops {
                    true => format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nLocation: {target}\r\n\
                         Content-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                    false => "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\
                              Connection: close\r\n\r\n"
                        .to_string(),
                };
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        addr
    }

    /// The client itself, following a real redirect: a public-looking endpoint
    /// answering `307 Location: http://169.254.169.254/…` reaches the metadata
    /// service unless every hop is re-checked, and 307 preserves the method and
    /// the body.
    #[tokio::test(flavor = "multi_thread")]
    async fn checked_client_refuses_a_redirect_to_a_restricted_address() {
        let addr = redirecting_server("http://169.254.169.254/latest/".to_string(), 1).await;

        // The policy only sees *redirects*, never the URL the caller passed, so
        // the initial connect to loopback proceeds regardless of this flag -
        // what it governs is whether the hop onward to link-local is followed.
        let client = checked_client(ClientTimeouts::default(), false);
        let err = client
            .get(format!("http://{addr}/hook"))
            .send()
            .await
            .expect_err("the hop to 169.254.169.254 must be refused");

        // reqwest wraps the policy's message in its source chain, so the reason
        // lives below the top-level "error sending request".
        let mut chain = err.to_string();
        let mut src: Option<&dyn std::error::Error> = std::error::Error::source(&err);
        while let Some(e) = src {
            chain.push_str(&format!(": {e}"));
            src = e.source();
        }
        let refused = chain.contains("refused to follow redirect");
        assert!(refused, "{chain}");
    }

    /// And a permitted hop is actually followed - so the policy is deciding per
    /// address rather than refusing every redirect.
    #[tokio::test(flavor = "multi_thread")]
    async fn checked_client_follows_a_permitted_redirect() {
        // Somewhere real to land, then a server that points at it once.
        let destination = redirecting_server(String::new(), 0).await;
        let addr = redirecting_server(format!("http://{destination}/done"), 1).await;

        let client = checked_client(ClientTimeouts::default(), true);
        let resp = client
            .get(format!("http://{addr}/hook"))
            .send()
            .await
            .expect("a permitted hop is followed to its destination");
        assert_eq!(resp.status().as_u16(), 204);
    }

    /// A hop is a destination the caller's original check never saw. 307/308
    /// preserve the method *and* the body, so a public endpoint answering with a
    /// redirect to a private address turns a webhook into a request primitive
    /// against the internal network - capping the hop count does not help,
    /// because the first hop is already somewhere else.
    #[test]
    fn a_redirect_hop_is_checked_like_any_other_url() {
        let private = "http://169.254.169.254/latest/".parse().unwrap();
        assert!(redirect_decision(0, &private, false).is_err());
        assert!(redirect_decision(0, &"http://127.0.0.1/x".parse().unwrap(), false).is_err());

        // The deliberate opt-out still works, so the check reads the address
        // rather than refusing every redirect.
        assert!(redirect_decision(0, &private, true).is_ok());
    }

    /// And a loop is bounded even when every hop is permitted, so a server
    /// cannot hold a delivery open by bouncing it forever.
    #[test]
    fn a_redirect_loop_is_bounded_regardless_of_destination() {
        let ok = "http://127.0.0.1/x".parse().unwrap();
        assert!(redirect_decision(4, &ok, true).is_ok());
        let err = redirect_decision(5, &ok, true).expect_err("the sixth hop is refused");
        assert!(err.contains("too many redirects"), "{err}");
    }

    fn u(s: &str) -> url::Url {
        url::Url::parse(s).expect("test URL parses")
    }

    /// The floor exists because a bare `Client::new()` carries no timeouts at
    /// all.
    #[test]
    fn the_default_timeouts_are_a_real_floor() {
        let t = ClientTimeouts::default();
        assert!(t.connect > Duration::ZERO, "a connect timeout is the point");
        assert!(
            t.total > t.connect,
            "the total budget must exceed the connect"
        );
    }

    /// The builder has to actually produce a client - a factory returning a
    /// builder nothing can `build()` would be a silent no-op.
    #[test]
    fn the_client_builder_produces_a_usable_client() {
        assert!(client_builder(ClientTimeouts::default()).build().is_ok());
        // And a caller may override the floor with its own numbers.
        let custom = ClientTimeouts {
            connect: Duration::from_secs(1),
            total: Duration::from_secs(2),
        };
        assert!(client_builder(custom).build().is_ok());
        // The convenience wrapper callers actually use.
        let _ = client(ClientTimeouts::default());
    }

    #[test]
    fn rejects_non_http_schemes() {
        for s in ["file:///etc/passwd", "ftp://example.com/x", "gopher://x/"] {
            let err = check_url(&u(s), false).unwrap_err();
            assert_eq!(err.kind(), "scheme", "{s} → {err:?}");
        }
    }

    /// The headline case: an agent talked into fetching the cloud metadata
    /// endpoint gets credentials for the whole instance.
    #[test]
    fn rejects_cloud_metadata_endpoint() {
        // Under a subscriber, so the refusal's `debug!` body runs too.
        leviath_testkit::with_tracing(|| {
            let err = check_url(&u("http://169.254.169.254/latest/meta-data/"), false).unwrap_err();
            assert_eq!(err.kind(), "private_address");
        });
    }

    /// Asserting on `PrivateAddress` specifically, not just `is_err()`: an IPv6
    /// literal refused as *unresolvable* (the brackets from `host_str()`
    /// parsing as neither an address nor a name) would pass an `is_err()` check
    /// while the address rules never ran at all.
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
            assert_eq!(err.kind(), "private_address", "{s} → {err:?}");
        }
    }

    /// `::ffff:127.0.0.1` is loopback wearing a v6 costume.
    #[test]
    fn rejects_ipv4_mapped_loopback() {
        let err = check_url(&u("http://[::ffff:127.0.0.1]/"), false).unwrap_err();
        assert_eq!(err.kind(), "private_address", "{err:?}");
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
        assert_eq!(err.kind(), "unresolvable", "{err:?}");
    }

    /// `localhost` is a *name*, not a literal, so it exercises the resolver path
    /// rather than the parse-as-IP shortcut.
    #[test]
    fn rejects_localhost_by_name() {
        let err = check_url(&u("http://localhost:8080/"), false).unwrap_err();
        assert_eq!(err.kind(), "private_address", "{err:?}");
    }

    // ─── the resolver seam ────────────────────────────────────────────────

    fn resolves_to_nothing(_: &str, _: u16) -> Option<Vec<IpAddr>> {
        Some(Vec::new())
    }
    fn resolver_fails(_: &str, _: u16) -> Option<Vec<IpAddr>> {
        None
    }
    fn resolves_public(_: &str, _: u16) -> Option<Vec<IpAddr>> {
        Some(vec!["93.184.216.34".parse().expect("valid")])
    }
    fn resolves_to_both(_: &str, _: u16) -> Option<Vec<IpAddr>> {
        Some(vec![
            "93.184.216.34".parse().expect("valid"),
            "127.0.0.1".parse().expect("valid"),
        ])
    }

    /// A name that resolves to a public address passes. Injected rather than
    /// using real DNS: a test that depends on someone else's zone file is slow
    /// when it works and confusing when it doesn't.
    #[test]
    fn a_hostname_resolving_publicly_is_allowed() {
        assert!(check_url_with(&u("https://example.com/x"), false, resolves_public).is_ok());
    }

    /// A name with **both** a public and a private answer is refused rather than
    /// raced. Which address the client would actually dial is not ours to
    /// predict, so any restricted answer settles it.
    #[test]
    fn a_hostname_resolving_to_both_public_and_private_is_refused() {
        let err = check_url_with(&u("https://split.example/x"), false, resolves_to_both)
            .expect_err("a private answer must settle it");
        assert_eq!(err.kind(), "private_address");
    }

    /// A resolver error and an empty answer are the same thing to a caller:
    /// there is no address to check.
    #[test]
    fn no_addresses_reads_as_unresolvable() {
        for resolver in [resolves_to_nothing, resolver_fails] {
            let err = check_url_with(&u("https://nowhere.example/x"), false, resolver)
                .expect_err("no address means no fetch");
            assert_eq!(err.kind(), "unresolvable");
        }
    }

    /// How many times [`counting_resolver`] has been called. A counter rather
    /// than a panicking "must not run" resolver: the body of a resolver that is
    /// never called is itself an uncovered region, so the assertion would come
    /// at the cost of the thing it is asserting about.
    static RESOLVER_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn counting_resolver(_: &str, _: u16) -> Option<Vec<IpAddr>> {
        RESOLVER_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(vec!["93.184.216.34".parse().expect("valid")])
    }

    /// The scheme check runs before resolution, so a refused scheme never
    /// reaches the resolver - no DNS lookup for a URL we were never going to
    /// fetch.
    #[test]
    fn the_scheme_is_checked_before_anything_is_resolved() {
        use std::sync::atomic::Ordering::SeqCst;

        // Establish that this resolver does get called for a URL that reaches
        // resolution, so the count below means "not reached" and not "broken".
        let before = RESOLVER_CALLS.load(SeqCst);
        assert!(check_url_with(&u("https://example.com/x"), false, counting_resolver).is_ok());
        assert_eq!(RESOLVER_CALLS.load(SeqCst), before + 1);

        let err = check_url_with(&u("ftp://example.com/x"), false, counting_resolver)
            .expect_err("ftp is refused");
        assert_eq!(err.kind(), "scheme");
        assert_eq!(
            RESOLVER_CALLS.load(SeqCst),
            before + 1,
            "a refused scheme must not trigger a DNS lookup"
        );

        // A literal address short-circuits resolution too.
        assert!(check_url_with(&u("https://93.184.216.34/x"), false, counting_resolver).is_ok());
        assert_eq!(
            RESOLVER_CALLS.load(SeqCst),
            before + 1,
            "an IP literal must not trigger a DNS lookup"
        );
    }

    /// Every rejection reports a distinct kind, so the accessor is a real
    /// discriminator rather than a constant.
    #[test]
    fn each_rejection_has_its_own_kind() {
        let kinds = [
            UrlRejection::Scheme("ftp".into()).kind(),
            UrlRejection::Unresolvable("h".into()).kind(),
            UrlRejection::PrivateAddress {
                host: "h".into(),
                addr: "127.0.0.1".parse().expect("valid"),
            }
            .kind(),
        ];
        let unique: std::collections::HashSet<_> = kinds.iter().collect();
        assert_eq!(
            unique.len(),
            kinds.len(),
            "kinds must be distinct: {kinds:?}"
        );
    }

    #[test]
    fn rejection_messages_name_the_fix() {
        let err = check_url(&u("http://127.0.0.1/"), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("allow_local_network"), "{msg}");
        assert!(msg.contains("127.0.0.1"), "{msg}");

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
