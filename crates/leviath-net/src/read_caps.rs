//! How much the daemon will buffer from a remote peer before giving up.
//!
//! A reader of a provider body, a streamed frame or an MCP line accumulates
//! until the peer stops sending. A peer that never stops (a misbehaving
//! gateway, a compromised MCP server, a proxy replaying a file) would grow the
//! daemon's heap until the kernel kills it, taking every run with it. The caps
//! here are fixed numbers, not config keys: each is far above anything a
//! well-formed reply needs, so a user has no reason to raise one, and a knob
//! that only ever matters under attack is a knob an attacker gets to reason
//! about.
//!
//! Over a cap is an error to the caller, never a hang and never a panic. The
//! message names the cap and the peer, so a log line says which endpoint
//! misbehaved.

use std::fmt;

/// The most the daemon reads of one buffered HTTP body (a JSON reply from a
/// provider, an MCP HTTP response, an OAuth token exchange) before failing the
/// call: 64 MiB.
///
/// The largest legitimate body on this path is a model listing from a big
/// gateway, at well under 1 MiB, and a whole-turn reply from a 1M-context
/// model tops out at a few MiB.
pub const JSON_BODY_CAP: usize = 64 * 1024 * 1024;

/// The most one streamed frame, or one accumulated partial line, may hold
/// before the stream fails: 8 MiB.
///
/// A streaming parser keeps whatever has arrived since the last frame
/// boundary. A peer that never sends a boundary (no `\n\n`, no newline) keeps
/// that buffer growing, and a single frame that large is not one any wire
/// format the daemon speaks produces: an SSE delta is a few hundred bytes and
/// an NDJSON line is one chunk of a reply.
pub const STREAM_FRAME_CAP: usize = 8 * 1024 * 1024;

/// The longest line an MCP stdio server may write before the read fails:
/// 1 MiB.
///
/// A JSON-RPC frame is one line. A tool result of a megabyte is already far
/// past what a model can take in, and the runtime clips tool output well below
/// that; a longer line is a server streaming a file where a frame should be.
pub const MCP_LINE_CAP: usize = 1024 * 1024;

/// A cap as a person reads it: `64 MiB`, `1 MiB`, or `4096 bytes` for a
/// figure that is not a whole number of MiB (which only test-sized caps are).
pub fn describe_cap(cap: usize) -> String {
    const MIB: usize = 1024 * 1024;
    if cap >= MIB && cap.is_multiple_of(MIB) {
        format!("{} MiB", cap / MIB)
    } else {
        format!("{cap} bytes")
    }
}

/// The name a cap error reports for a response: the host it came from.
///
/// Every URL `reqwest` will send to has a host, so the fallback is for a
/// response built by hand in a test rather than one that came off a socket.
pub fn peer_of(response: &reqwest::Response) -> String {
    response
        .url()
        .host_str()
        .unwrap_or("an unnamed peer")
        .to_string()
}

/// Why a capped body read stopped short of a body.
#[derive(Debug)]
pub enum BodyReadError {
    /// The peer sent more than `cap` bytes; the read stopped there.
    TooLarge {
        /// The cap that was hit.
        cap: usize,
        /// Who sent it, from [`peer_of`].
        peer: String,
    },
    /// The connection failed before the body finished.
    Transport(reqwest::Error),
}

impl fmt::Display for BodyReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { cap, peer } => {
                write!(
                    f,
                    "response body exceeded {} from {peer}",
                    describe_cap(*cap)
                )
            }
            Self::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BodyReadError {}

/// Read a response body to the end, or to `cap` bytes, whichever comes first.
///
/// Chunk by chunk with a running total rather than `bytes()` followed by a
/// length check: the check after the fact only fires once the whole body is
/// already on the heap, which is the allocation the cap exists to prevent.
/// The connection is dropped with the response on the way out of the error
/// arm, so the peer's remaining bytes are never read.
pub async fn read_body_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, BodyReadError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(BodyReadError::Transport)? {
        if body.len() + chunk.len() > cap {
            return Err(BodyReadError::TooLarge {
                cap,
                peer: peer_of(&response),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// [`read_body_capped`], decoded the way `reqwest::Response::text` decodes:
/// by the `charset` the `Content-Type` header names, UTF-8 when it names
/// none, with replacement characters for what does not decode.
pub async fn read_text_capped(
    response: reqwest::Response,
    cap: usize,
) -> Result<String, BodyReadError> {
    let charset = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(charset_of)
        .unwrap_or_else(|| "utf-8".to_string());
    let body = read_body_capped(response, cap).await?;
    let encoding =
        encoding_rs::Encoding::for_label(charset.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(&body);
    Ok(text.into_owned())
}

/// The `charset=` parameter of a `Content-Type` value, unquoted.
fn charset_of(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (key, value) = param.trim().split_once('=')?;
        key.eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_string())
    })
}

/// The error a streaming parser returns once its partial-frame buffer has
/// grown past `cap`, or `Ok` while it has not.
pub fn frame_within_cap(buffered: usize, cap: usize, peer: &str) -> Result<(), String> {
    if buffered > cap {
        Err(format!(
            "stream frame exceeded {} from {peer}",
            describe_cap(cap)
        ))
    } else {
        Ok(())
    }
}

/// The message for a line a stdio peer kept writing past `cap`.
pub fn line_cap_message(cap: usize, peer: &str) -> String {
    format!("line exceeded {} from {peer}", describe_cap(cap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve one connection with `headers` and then `body_len` bytes of
    /// `x`, written in small pieces. The body is never all in one chunk, so
    /// the running total is what stops the read.
    async fn serve_body(headers: &'static str, body_len: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(headers.as_bytes()).await;
            let mut sent = 0;
            while sent < body_len {
                let piece = (body_len - sent).min(1024);
                // Errors are ignored: a client that stopped reading at its
                // cap closes the socket, and that is the test passing.
                let _ = sock.write_all(&vec![b'x'; piece]).await;
                sent += piece;
            }
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}/reply")
    }

    #[test]
    fn caps_are_described_in_mib_or_bytes() {
        assert_eq!(describe_cap(JSON_BODY_CAP), "64 MiB");
        assert_eq!(describe_cap(STREAM_FRAME_CAP), "8 MiB");
        assert_eq!(describe_cap(MCP_LINE_CAP), "1 MiB");
        assert_eq!(describe_cap(4096), "4096 bytes");
    }

    #[tokio::test]
    async fn a_body_under_the_cap_is_read_whole() {
        let url = serve_body(
            "HTTP/1.1 200 OK\r\nContent-Length: 3000\r\nConnection: close\r\n\r\n",
            3000,
        )
        .await;
        let response = reqwest::get(url).await.unwrap();
        let body = read_text_capped(response, 4096).await.unwrap();
        assert_eq!(body.len(), 3000);
    }

    #[tokio::test]
    async fn a_body_over_the_cap_fails_naming_the_cap_and_the_peer() {
        let url = serve_body(
            "HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
            10000,
        )
        .await;
        let response = reqwest::get(url).await.unwrap();
        let err = read_body_capped(response, 4096).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "response body exceeded 4096 bytes from 127.0.0.1"
        );
    }

    #[tokio::test]
    async fn text_past_the_cap_is_the_same_error() {
        let url = serve_body(
            "HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
            10000,
        )
        .await;
        let response = reqwest::get(url).await.unwrap();
        let err = read_text_capped(response, 4096).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "response body exceeded 4096 bytes from 127.0.0.1"
        );
    }

    #[tokio::test]
    async fn a_body_that_stops_early_is_a_transport_failure() {
        // Declares more than it sends, then hangs up.
        let url = serve_body(
            "HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
            100,
        )
        .await;
        let response = reqwest::get(url).await.unwrap();
        let err = read_body_capped(response, 4096).await.unwrap_err();
        // Told apart by what it says: the one message this module writes
        // itself is the cap's, and a transport failure is reqwest's own.
        assert!(!err.to_string().starts_with("response body exceeded"));
        assert!(!err.to_string().is_empty());
        assert!(std::error::Error::source(&err).is_none());
    }

    #[tokio::test]
    async fn the_peer_is_the_host_or_the_whole_url() {
        let url = serve_body(
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            0,
        )
        .await;
        let response = reqwest::get(url).await.unwrap();
        assert_eq!(peer_of(&response), "127.0.0.1");
    }

    #[tokio::test]
    async fn text_is_decoded_by_the_declared_charset() {
        // `caf\xe9` in Latin-1; read as UTF-8 it would be a replacement
        // character, which is what `Response::text` also avoids.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=\"iso-8859-1\"\r\n\
                      Content-Length: 4\r\nConnection: close\r\n\r\ncaf\xe9",
                )
                .await;
            let _ = sock.shutdown().await;
        });
        let response = reqwest::get(format!("http://{addr}/")).await.unwrap();
        assert_eq!(read_text_capped(response, 4096).await.unwrap(), "caf\u{e9}");
    }

    #[test]
    fn the_charset_parameter_is_found_or_absent() {
        assert_eq!(
            charset_of("text/html; Charset=UTF-8").as_deref(),
            Some("UTF-8")
        );
        assert_eq!(charset_of("application/json"), None);
        assert_eq!(charset_of("text/plain; boundary=x"), None);
        assert_eq!(charset_of("text/plain; nonsense"), None);
    }

    #[test]
    fn frames_and_lines_are_judged_against_the_cap() {
        assert!(frame_within_cap(8, 8, "openai").is_ok());
        assert_eq!(
            frame_within_cap(9, 8, "openai").unwrap_err(),
            "stream frame exceeded 8 bytes from openai"
        );
        assert_eq!(
            line_cap_message(MCP_LINE_CAP, "mcp server 'fs'"),
            "line exceeded 1 MiB from mcp server 'fs'"
        );
    }
}
