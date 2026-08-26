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
            // Only the phrases these libraries actually emit, taken from real
            // failures rather than guessed. An earlier version of this looked
            // for "tls" and "certificate" and caught neither: rustls answers a
            // plaintext port with "received corrupt message of type
            // invalidcontenttype", which contains no word anyone would think to
            // search for.
            //
            // Anything unrecognised stays `Transport` rather than being folded
            // into the nearest guess. A wrong remedy sends somebody to check
            // their certificates when the port was closed, which is worse than
            // "could not be reached".
            return connect_failure(&error_chain_text(e));
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

/// Place a connect-stage failure by what its cause chain says.
///
/// A free function taking the text, so every arm is reachable from a test: a
/// chain that matches nothing cannot be produced from a real socket on demand,
/// and it is the arm that most needs to be right - it is what stops an
/// unrecognised failure being folded into the nearest guess.
///
/// The phrases are ones these libraries actually emit, taken from real failures.
/// An earlier version looked for "tls" and "certificate" and caught neither:
/// rustls answers a plaintext port with "received corrupt message of type
/// invalidcontenttype", which contains no word anyone would think to search for.
fn connect_failure(chain: &str) -> FailureKind {
    const DNS: [&str; 3] = [
        "dns error",
        "failed to lookup address",
        "name or service not known",
    ];
    const TLS: [&str; 6] = [
        "certificate",
        "corrupt message",
        "handshake",
        "tls",
        "unknown issuer",
        "protocol version",
    ];
    const REFUSED: [&str; 3] = ["connection refused", "os error 61", "os error 111"];

    if DNS.iter().any(|p| chain.contains(p)) {
        return FailureKind::DnsFailure;
    }
    if TLS.iter().any(|p| chain.contains(p)) {
        return FailureKind::TlsFailure;
    }
    if REFUSED.iter().any(|p| chain.contains(p)) {
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
    use super::{FailureKind, connect_failure};

    /// Each family, by a phrase these libraries really emit.
    #[test]
    fn each_recognised_family_is_placed() {
        for chain in [
            "client error (connect) | dns error: failed to lookup address information",
            "name or service not known",
        ] {
            assert_eq!(connect_failure(chain), FailureKind::DnsFailure, "{chain}");
        }
        for chain in [
            "received corrupt message of type invalidcontenttype",
            "invalid peer certificate: unknown issuer",
            "handshake failed",
            "tls error",
            "peer misbehaved: protocol version",
        ] {
            assert_eq!(connect_failure(chain), FailureKind::TlsFailure, "{chain}");
        }
        for chain in [
            "tcp connect error: connection refused (os error 61)",
            "os error 111",
        ] {
            assert_eq!(
                connect_failure(chain),
                FailureKind::ConnectionRefused,
                "{chain}"
            );
        }
    }

    /// The arm that matters most and cannot be produced from a real socket: a
    /// failure this build does not recognise stays `Transport` rather than being
    /// folded into the nearest guess, because a wrong remedy is worse than a
    /// vague one.
    #[test]
    fn an_unrecognised_chain_is_not_guessed_at() {
        assert_eq!(
            connect_failure("something nobody here has seen before"),
            FailureKind::Transport
        );
        assert_eq!(connect_failure(""), FailureKind::Transport);
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
