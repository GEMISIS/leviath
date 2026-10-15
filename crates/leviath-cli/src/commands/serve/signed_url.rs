//! Short-lived signed URLs for the byte routes.
//!
//! The byte routes take a bearer token, and a browser cannot put a header on an
//! `<img src>` or a download link. So a console that wanted to show a run's
//! screenshot had to fetch it with the token, hold the bytes in memory and mint
//! a blob URL, which is a great deal of machinery to look at a picture.
//!
//! A signed URL is the alternative: a URL that carries its own permission, for
//! one path, for a few minutes. What it deliberately is not is the API token in
//! a query string. A URL ends up in proxy logs, browser history and `Referer`
//! headers, and this one is worth nothing beyond the one file it names and the
//! minute it was minted in.

use std::sync::Arc;

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// How long a minted URL is good for.
///
/// Long enough for a page to load the images on it, short enough that a URL
/// copied out of a log is worthless by the time anybody reads it. Not
/// configurable: a knob here would mostly be a way to make it worse.
pub(super) const URL_TTL_SECS: i64 = 300;

/// The signer for one server process.
///
/// The key is random per process and never written down, so every URL minted by
/// a server stops working when it restarts. That is the intent: these are
/// links for a page that is open now, not capabilities to keep.
pub(super) struct UrlSigner {
    key: [u8; 32],
}

impl Default for UrlSigner {
    fn default() -> Self {
        use rand::RngExt as _;
        // 256 bits from the OS generator, the same way the daemon's control
        // token is minted. Never written down: a restart is meant to invalidate
        // every URL this process handed out.
        Self {
            key: rand::rng().random(),
        }
    }
}

impl UrlSigner {
    /// Mint a query string granting this path, with this query, until `exp`.
    ///
    /// `query` is the rest of the URL's query exactly as it will be sent,
    /// already encoded, and empty when there is none. Returns the
    /// `exp=…&sig=…` pair, without a leading `?` or `&`, so a caller composes
    /// it onto that query.
    pub(super) fn sign(&self, path: &str, query: &str, now: i64) -> String {
        let expires = now + URL_TTL_SECS;
        let signature = self.signature(path, query, expires);
        format!("exp={expires}&sig={signature}")
    }

    /// Whether this query grants `path` at `now`.
    ///
    /// Both halves must be there, the expiry must be in the future, and the
    /// signature must be over this exact path and the rest of this query: a
    /// signature for one run's blob does not open another's, and a link to
    /// one file in a working directory does not open its neighbour.
    pub(super) fn verify(&self, path: &str, query: Option<&str>, now: i64) -> bool {
        let Some(query) = query else {
            return false;
        };
        let mut expires = None;
        let mut presented = None;
        let mut signed = Vec::new();
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("exp=") {
                expires = value.parse::<i64>().ok();
            } else if let Some(value) = pair.strip_prefix("sig=") {
                presented = Some(value);
            } else if is_signed(pair) {
                signed.push(pair);
            }
        }
        let (Some(expires), Some(presented)) = (expires, presented) else {
            return false;
        };
        if expires <= now {
            return false;
        }
        leviath_core::constant_time_eq(presented, &self.signature(path, &signed.join("&"), expires))
    }

    /// The signature over one path, its query and expiry.
    ///
    /// The path is inside the signed message, which is what keeps a URL to one
    /// route: without it, a signature minted for a readable file would open
    /// every other route the byte handler serves. The query is in it too,
    /// because `files/raw` names its file there.
    fn signature(&self, path: &str, query: &str, expires: i64) -> String {
        let mut mac = <Hmac<Sha256>>::new_from_slice(&self.key)
            .expect("HMAC-SHA256 accepts a key of any length");
        mac.update(path.as_bytes());
        mac.update(b"\n");
        mac.update(expires.to_string().as_bytes());
        mac.update(b"\n");
        mac.update(query.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

/// Whether a query pair is inside the signature.
///
/// Every pair is but `download`, which changes only whether the browser saves
/// the file or shows it. A console that adds it to a link it was handed opens
/// nothing it could not open already.
fn is_signed(pair: &str) -> bool {
    pair != "download" && !pair.starts_with("download=")
}

/// Percent-encode one path segment or query value of a minted link.
///
/// A file or artifact name can hold a space, `&`, `#` or `?`, and written into
/// a link as it is, each of those ends the name early or moves the rest of it
/// somewhere else. Every byte but the unreserved ones is encoded, which reads
/// the same in a path and in a query.
pub(super) fn encode(part: &str) -> String {
    leviath_scripting::tool::percent_encode(part)
}

/// The routes a signed URL may open.
///
/// Bytes only. A signed URL is for showing a file in a page, so it opens the
/// routes that serve file bytes and nothing else: no listing, no run record, no
/// config, and nothing that writes.
pub(super) fn is_signable(path: &str) -> bool {
    let bytes = path.starts_with("/api/runs/")
        && (path.contains("/blobs/")
            || path.contains("/artifacts/")
            || path.ends_with("/files/raw"));
    bytes || path.starts_with("/api/exports/")
}

/// Mint a full URL path for one byte route.
///
/// `path` is sent as it is, so a caller encodes any segment it built from a
/// name with [`encode`]. The values in `extra_query` are encoded here, and
/// signed with the path.
///
/// Relative on purpose: the caller reached this server somehow, and a path
/// keeps whatever host, scheme and port that was. A server that guessed its own
/// public URL would guess wrong behind a proxy.
pub(super) fn signed_path(
    signer: &Arc<UrlSigner>,
    path: &str,
    extra_query: &[(&str, &str)],
    now: i64,
) -> String {
    let query = extra_query
        .iter()
        .map(|(name, value)| format!("{name}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let signed = query
        .split('&')
        .filter(|pair| is_signed(pair))
        .collect::<Vec<_>>()
        .join("&");
    let grant = signer.sign(path, &signed, now);
    match query.is_empty() {
        true => format!("{path}?{grant}"),
        false => format!("{path}?{grant}&{query}"),
    }
}

#[cfg(test)]
#[path = "signed_url_tests.rs"]
mod tests;
