//! What a failed provider call actually was.
//!
//! Separate from [`crate::provider`] for the same reason [`crate::capabilities`]
//! is: that module is how a provider is *called*, this one is what came back
//! when the call did not work. They meet where a `ProviderError` carries a
//! [`FailureKind`].

use serde::{Deserialize, Serialize};

use crate::provider::ProviderError;

/// What actually went wrong when a provider call failed, at the granularity a
/// person debugging it needs.
///
/// The remedy differs per variant and nothing else in the error carries it. A
/// failed call used to arrive as one of two strings - a transport failure or an
/// HTTP status - and "could not reach the provider" covered a wrong base URL, a
/// firewall, an expired certificate and a laptop that had gone to sleep. Each
/// wants a different thing done about it, and the message was the same.
///
/// Recorded on the error, logged as a field, and reported by the API, so the
/// question "was that my key, my URL, or their outage" has an answer that does
/// not require reading a raw provider body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The name did not resolve. A typo in a base URL, or no DNS at all.
    DnsFailure,
    /// The host resolved and refused, or nothing was listening. A local
    /// provider that is not running, or a port that is wrong.
    ConnectionRefused,
    /// The TLS handshake failed: an expired or untrusted certificate, or a
    /// proxy intercepting the connection with its own.
    TlsFailure,
    /// The request went out and no answer came back in time. Distinct from a
    /// refusal: something is listening and is either slow or wedged.
    Timeout,
    /// The connection died mid-body. The provider answered and the answer did
    /// not finish arriving.
    ConnectionDropped,
    /// A transport failure this build cannot place more precisely.
    Transport,
    /// The provider answered, and the answer says the fault is ours: a
    /// malformed request, an unknown parameter, a model name it does not carry.
    /// Retrying unchanged produces the same answer.
    BadRequest,
    /// The provider answered 404. Almost always a base URL pointing at the
    /// wrong path, or a model that does not exist on this endpoint - and worth
    /// its own variant because it reads as "the provider is broken" when it is
    /// usually a line of config.
    NotFound,
    /// The provider answered 5xx. Their fault, not ours, and the same request
    /// may well work in a minute.
    ServerError,
    /// The provider answered something this build could not parse.
    MalformedResponse,
}

impl FailureKind {
    /// A short, stable label for logs, metrics attributes and the API.
    pub fn label(self) -> &'static str {
        match self {
            FailureKind::DnsFailure => "dns-failure",
            FailureKind::ConnectionRefused => "connection-refused",
            FailureKind::TlsFailure => "tls-failure",
            FailureKind::Timeout => "timeout",
            FailureKind::ConnectionDropped => "connection-dropped",
            FailureKind::Transport => "transport",
            FailureKind::BadRequest => "bad-request",
            FailureKind::NotFound => "not-found",
            FailureKind::ServerError => "server-error",
            FailureKind::MalformedResponse => "malformed-response",
        }
    }

    /// What to do about it, in one line, for whoever has to fix it.
    pub fn remedy(self) -> &'static str {
        match self {
            FailureKind::DnsFailure => {
                "the provider's hostname did not resolve - check `base_url` for a typo, and \
                 that this machine has DNS"
            }
            FailureKind::ConnectionRefused => {
                "nothing accepted the connection - check the provider is running and that \
                 `base_url` names the right port"
            }
            FailureKind::TlsFailure => {
                "the TLS handshake failed - an expired or untrusted certificate, or a proxy \
                 presenting its own"
            }
            FailureKind::Timeout => {
                "the provider did not answer in time - it is reachable but slow or wedged; \
                 `request_timeout_secs` raises the wait"
            }
            FailureKind::ConnectionDropped => {
                "the connection died before the answer finished arriving"
            }
            FailureKind::Transport => "the provider could not be reached",
            FailureKind::BadRequest => {
                "the provider rejected the request itself - a parameter it does not accept, \
                 or a model name it does not carry"
            }
            FailureKind::NotFound => {
                "the endpoint answered 404 - usually `base_url` pointing at the wrong path, \
                 or a model that does not exist there"
            }
            FailureKind::ServerError => {
                "the provider's own endpoint failed; this may pass on a retry"
            }
            FailureKind::MalformedResponse => {
                "the provider answered with something this build could not parse"
            }
        }
    }

    /// Place a `reqwest` failure, which knows more than its `Display` says.
    ///
    /// Every one of these arrived as `RequestFailed(e.to_string())` and read as
    /// the same thing. `reqwest` distinguishes them and the information was
    /// being thrown away one line after it was available.
    pub fn from_reqwest(e: &reqwest::Error) -> Self {
        if e.is_timeout() {
            return FailureKind::Timeout;
        }
        if e.is_body() || e.is_decode() {
            return FailureKind::ConnectionDropped;
        }
        if e.is_connect() {
            // `is_connect` covers everything that failed before a request was
            // sent, so the cause chain is what separates a name that did not
            // resolve from a certificate that was not trusted. Matched on text
            // because the concrete error types live in private dependencies of
            // `reqwest` and are not nameable from here.
            //
            // Anything unrecognised stays `Transport` rather than being folded
            // into the nearest guess. A wrong remedy sends somebody to check
            // their certificates when the port was closed, which is worse than
            // "could not be reached".
            return connect_failure(io_error_kind(e), &error_chain_text(e));
        }
        FailureKind::Transport
    }

    /// Place an HTTP status the provider answered with.
    ///
    /// Only the statuses that are not already an [`crate::provider::UnavailableReason`]: 401,
    /// 402, 403 and 429 are handled before this is reached, because they mean
    /// the provider is unusable rather than that one request went wrong.
    pub fn from_status(status: u16) -> Self {
        match status {
            404 => FailureKind::NotFound,
            500..=599 => FailureKind::ServerError,
            _ => FailureKind::BadRequest,
        }
    }
}

/// The `std::io::ErrorKind` underneath a `reqwest` failure, when there is one.
///
/// A refused connection arrives as a real `io::Error` three layers down the
/// cause chain, and its `kind()` is the same value on every platform. The text
/// is not: the same refusal reads "connection refused (os error 61)" on macOS,
/// "os error 111" on Linux, and "No connection could be made because the target
/// machine actively refused it. (os error 10061)" on Windows. Matching on the
/// kind is what makes this classifier say the same thing on all three.
fn io_error_kind(e: &(dyn std::error::Error + 'static)) -> Option<std::io::ErrorKind> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return Some(io.kind());
        }
        source = cause.source();
    }
    None
}

/// Place a connect-stage failure, by its `io::ErrorKind` where it has one and
/// by what its cause chain says where it does not.
///
/// A free function taking both, so every arm is reachable from a test: a chain
/// that matches nothing cannot be produced from a real socket on demand, and it
/// is the arm that most needs to be right - it is what stops an unrecognised
/// failure being folded into the nearest guess.
///
/// The kind is tried first because it is portable. Text is the fallback, and it
/// carries the cases the standard library has no kind for - a name that did not
/// resolve and a handshake that failed both arrive as `Uncategorized` or as no
/// `io::Error` at all. Those phrases are ones these libraries really emit, taken
/// from measured failures: an earlier version looked for "tls" and "certificate"
/// and caught neither, because rustls answers a plaintext port with "received
/// corrupt message of type invalidcontenttype", which contains no word anyone
/// would think to search for.
fn connect_failure(io: Option<std::io::ErrorKind>, chain: &str) -> FailureKind {
    use std::io::ErrorKind;

    const DNS: [&str; 4] = [
        "dns error",
        "failed to lookup address",
        "name or service not known",
        "no such host is known",
    ];
    const TLS: [&str; 6] = [
        "certificate",
        "corrupt message",
        "handshake",
        "tls",
        "unknown issuer",
        "protocol version",
    ];

    // A name that did not resolve can surface with a socket error underneath it
    // on some stacks, so the text is asked first where it is unambiguous.
    if DNS.iter().any(|p| chain.contains(p)) {
        return FailureKind::DnsFailure;
    }
    match io {
        Some(ErrorKind::ConnectionRefused) => return FailureKind::ConnectionRefused,
        Some(ErrorKind::TimedOut) => return FailureKind::Timeout,
        Some(ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted) => {
            return FailureKind::ConnectionDropped;
        }
        _ => {}
    }
    if TLS.iter().any(|p| chain.contains(p)) {
        return FailureKind::TlsFailure;
    }
    // Kept for a stack that reports a refusal with no `io::Error` to read.
    if chain.contains("connection refused") || chain.contains("actively refused") {
        return FailureKind::ConnectionRefused;
    }
    // Deliberately not "probably refused". A wrong remedy sends somebody to
    // check their certificates when the port was closed, which is worse than
    // "could not be reached".
    FailureKind::Transport
}

/// Every message in an error's cause chain, lowercased and joined.
///
/// `Display` on a `reqwest::Error` says only the outermost layer, which for a
/// connection failure is the same sentence whatever went wrong underneath. What
/// separates a DNS failure from a refused connection is further down.
fn error_chain_text(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string().to_ascii_lowercase();
    let mut source = e.source();
    while let Some(cause) = source {
        text.push_str(" | ");
        text.push_str(&cause.to_string().to_ascii_lowercase());
        source = cause.source();
    }
    text
}

impl ProviderError {
    /// A transport failure, with what `reqwest` knew about it kept.
    ///
    /// Every one of these used to be `RequestFailed(e.to_string())`, and
    /// `Display` on a `reqwest::Error` says the same sentence for a hostname
    /// that does not resolve, a port with nothing behind it, and a certificate
    /// nobody trusts. The kind is worked out here, where the typed error still
    /// exists, and carried in the message so it survives every layer that only
    /// passes strings.
    pub fn transport(context: &str, e: &reqwest::Error) -> Self {
        let kind = FailureKind::from_reqwest(e);
        ProviderError::RequestFailed(format!(
            "[{}] {context}: {e} - {}",
            kind.label(),
            kind.remedy()
        ))
    }

    /// The kind this error carries, when it names one.
    ///
    /// Read back out of the message rather than held in a field: these errors
    /// cross into a Rhai script and back as plain strings, and a field would be
    /// lost at that boundary while a prefix survives it.
    pub fn failure_kind(&self) -> Option<FailureKind> {
        let message = match self {
            ProviderError::RequestFailed(m) | ProviderError::ApiError(m) => m,
            ProviderError::InvalidResponse(_) => return Some(FailureKind::MalformedResponse),
            _ => return None,
        };
        let label = message.strip_prefix('[')?.split_once(']')?.0;
        [
            FailureKind::DnsFailure,
            FailureKind::ConnectionRefused,
            FailureKind::TlsFailure,
            FailureKind::Timeout,
            FailureKind::ConnectionDropped,
            FailureKind::Transport,
            FailureKind::BadRequest,
            FailureKind::NotFound,
            FailureKind::ServerError,
            FailureKind::MalformedResponse,
        ]
        .into_iter()
        .find(|k| k.label() == label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::build_http_client;

    /// A host that accepts the connection and never answers is a timeout, not a
    /// refusal - something is listening, it is just not replying. Told apart
    /// because the remedies differ: one is "check it is running", the other is
    /// "it is running and stuck".
    #[tokio::test]
    async fn a_server_that_never_answers_is_a_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let addr = listener.local_addr().expect("has an address");
        // Accept and hold, writing nothing.
        std::thread::spawn(move || {
            let _held = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(30));
        });

        let client = build_http_client(Some(1)).expect("a client builds");
        let e = client
            .get(format!("http://{addr}/v1/models"))
            .send()
            .await
            .expect_err("times out");

        assert_eq!(FailureKind::from_reqwest(&e), FailureKind::Timeout);
    }

    /// TLS spoken to a plain HTTP port fails the handshake, which is its own
    /// remedy - a certificate or a proxy, not a wrong port.
    #[tokio::test]
    async fn a_failed_handshake_is_told_from_a_refusal() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let addr = listener.local_addr().expect("has an address");
        std::thread::spawn(move || {
            // Accept and answer plaintext, so the handshake fails rather than
            // the connection being refused.
            use std::io::Write;
            let mut socket = listener.accept().expect("the client connects").0;
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
            // Held open. Returning here closes the socket, and a close with the
            // client's handshake still unread sends a reset that discards the
            // bytes just written - so rustls would report a dropped connection
            // rather than the plaintext it was meant to choke on.
            std::thread::sleep(std::time::Duration::from_secs(30));
        });

        let client = build_http_client(Some(5)).expect("a client builds");
        let e = client
            .get(format!("https://{addr}/v1/models"))
            .send()
            .await
            .expect_err("the handshake fails");

        assert_eq!(FailureKind::from_reqwest(&e), FailureKind::TlsFailure);
    }

    /// A message whose bracket never closes is not a prefix, and must not be
    /// read as one.
    #[test]
    fn an_unterminated_prefix_names_no_kind() {
        assert_eq!(
            ProviderError::RequestFailed("[never-closed and then some".to_string()).failure_kind(),
            None
        );
    }
}

#[cfg(test)]
mod connect_failure_tests {
    use super::{FailureKind, connect_failure, error_chain_text, io_error_kind};
    use std::io::ErrorKind;

    /// An error with nothing socket-shaped anywhere in its chain, which no real
    /// connect failure produces - every one of those has an `io::Error` a few
    /// layers down. Worth stating anyway: the answer is "nothing to read", not a
    /// guess, and the text is left to place the failure on its own.
    #[test]
    fn an_error_with_no_socket_beneath_it_claims_nothing() {
        #[derive(Debug)]
        struct Bare;
        impl std::fmt::Display for Bare {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("nothing socket-shaped here")
            }
        }
        impl std::error::Error for Bare {}

        // Put through the pair together, which is how it is really reached:
        // nothing to read from the kind, so the text has to place it alone.
        assert_eq!(io_error_kind(&Bare), None);
        assert_eq!(
            connect_failure(io_error_kind(&Bare), &error_chain_text(&Bare)),
            FailureKind::Transport
        );
    }

    /// And one that is buried rather than outermost, which is where a real one
    /// always sits: `reqwest` wraps it three deep.
    #[test]
    fn a_buried_socket_error_is_still_found() {
        #[derive(Debug)]
        struct Wrapper(std::io::Error);
        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("sending the request")
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let wrapped = Wrapper(std::io::Error::from(ErrorKind::ConnectionRefused));
        assert_eq!(io_error_kind(&wrapped), Some(ErrorKind::ConnectionRefused));
        // The chain text reaches the buried layer too, which is what the
        // fallback reads when the kind says nothing.
        let chain = error_chain_text(&wrapped);
        assert!(chain.contains("sending the request"), "{chain}");
        assert!(chain.contains("refused"), "{chain}");
    }

    /// The portable half. These are the kinds a socket really reports, and the
    /// reason this is matched at all: the same refusal is "os error 61" on
    /// macOS, "os error 111" on Linux and "os error 10061" on Windows, so a
    /// classifier built on the text alone answers differently per platform.
    #[test]
    fn a_socket_error_kind_places_the_failure_on_any_platform() {
        for (kind, expected) in [
            (ErrorKind::ConnectionRefused, FailureKind::ConnectionRefused),
            (ErrorKind::TimedOut, FailureKind::Timeout),
            (ErrorKind::ConnectionReset, FailureKind::ConnectionDropped),
            (ErrorKind::ConnectionAborted, FailureKind::ConnectionDropped),
        ] {
            // Deliberately with no text to lean on: the kind alone has to carry it.
            assert_eq!(connect_failure(Some(kind), ""), expected, "{kind:?}");
        }
    }

    /// A kind this does not place falls through to the text rather than being
    /// answered from the kind alone.
    #[test]
    fn an_unplaced_kind_defers_to_the_text() {
        assert_eq!(
            connect_failure(Some(ErrorKind::Other), "invalid peer certificate"),
            FailureKind::TlsFailure
        );
        assert_eq!(
            connect_failure(Some(ErrorKind::Other), "nothing recognisable"),
            FailureKind::Transport
        );
    }

    /// The text half, by phrases these libraries really emit. DNS and TLS have
    /// no `io::ErrorKind` of their own, so this is the only signal for them.
    #[test]
    fn each_recognised_family_is_placed_from_the_text() {
        for chain in [
            "client error (connect) | dns error: failed to lookup address information",
            "name or service not known",
            // Windows says this where Unix says "failed to lookup address".
            "no such host is known. (os error 11001)",
        ] {
            assert_eq!(
                connect_failure(None, chain),
                FailureKind::DnsFailure,
                "{chain}"
            );
        }
        for chain in [
            "received corrupt message of type invalidcontenttype",
            "invalid peer certificate: unknown issuer",
            "handshake failed",
            "tls error",
            "peer misbehaved: protocol version",
        ] {
            assert_eq!(
                connect_failure(None, chain),
                FailureKind::TlsFailure,
                "{chain}"
            );
        }
        for chain in [
            "tcp connect error: connection refused (os error 61)",
            // Windows phrasing, which shares no word with the Unix one.
            "no connection could be made because the target machine actively \
             refused it. (os error 10061)",
        ] {
            assert_eq!(
                connect_failure(None, chain),
                FailureKind::ConnectionRefused,
                "{chain}"
            );
        }
    }

    /// A resolution failure is named as one even when a socket error sits
    /// underneath it, because "check the hostname" and "check the port" send
    /// somebody to different places.
    #[test]
    fn a_name_that_did_not_resolve_outranks_the_socket_kind() {
        assert_eq!(
            connect_failure(
                Some(ErrorKind::ConnectionRefused),
                "dns error: failed to lookup address information"
            ),
            FailureKind::DnsFailure
        );
    }

    /// The arm that matters most and cannot be produced from a real socket: a
    /// failure this build does not recognise stays `Transport` rather than being
    /// folded into the nearest guess, because a wrong remedy is worse than a
    /// vague one.
    #[test]
    fn an_unrecognised_chain_is_not_guessed_at() {
        assert_eq!(
            connect_failure(None, "something nobody here has seen before"),
            FailureKind::Transport
        );
        assert_eq!(connect_failure(None, ""), FailureKind::Transport);
    }
}

#[cfg(test)]
mod fallthrough_tests {
    use super::FailureKind;
    use crate::provider::build_http_client;

    /// Not every `reqwest` failure is one of the four this can place. A request
    /// the builder refuses outright is none of them, and lands on the honest
    /// answer rather than the nearest one.
    #[tokio::test]
    async fn a_failure_that_is_none_of_the_known_shapes_stays_transport() {
        let client = build_http_client(Some(2)).expect("a client builds");
        // A scheme with no handler: refused before any connection is attempted,
        // so it is neither a timeout, a body, nor a connect failure.
        let e = client
            .get("ftp://example.invalid/models")
            .send()
            .await
            .expect_err("the builder refuses it");

        assert_eq!(FailureKind::from_reqwest(&e), FailureKind::Transport);
    }
}
