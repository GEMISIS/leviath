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
    /// Mint a query string granting this path until `exp`.
    ///
    /// Returns the `exp=…&sig=…` pair, without a leading `?` or `&`, so a
    /// caller composes it onto whatever query the route already has.
    pub(super) fn sign(&self, path: &str, now: i64) -> String {
        let expires = now + URL_TTL_SECS;
        let signature = self.signature(path, expires);
        format!("exp={expires}&sig={signature}")
    }

    /// Whether this query grants `path` at `now`.
    ///
    /// Both halves must be there, the expiry must be in the future, and the
    /// signature must be over this exact path: a signature for one run's blob
    /// does not open another's.
    pub(super) fn verify(&self, path: &str, query: Option<&str>, now: i64) -> bool {
        let Some(query) = query else {
            return false;
        };
        let mut expires = None;
        let mut presented = None;
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("exp=") {
                expires = value.parse::<i64>().ok();
            }
            if let Some(value) = pair.strip_prefix("sig=") {
                presented = Some(value);
            }
        }
        let (Some(expires), Some(presented)) = (expires, presented) else {
            return false;
        };
        if expires <= now {
            return false;
        }
        leviath_core::constant_time_eq(presented, &self.signature(path, expires))
    }

    /// The signature over one path and expiry.
    ///
    /// The path is inside the signed message, which is what keeps a URL to one
    /// file: without it, a signature minted for a readable file would open
    /// every other route the byte handler serves.
    fn signature(&self, path: &str, expires: i64) -> String {
        let mut mac = <Hmac<Sha256>>::new_from_slice(&self.key)
            .expect("HMAC-SHA256 accepts a key of any length");
        mac.update(path.as_bytes());
        mac.update(b"\n");
        mac.update(expires.to_string().as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

/// The routes a signed URL may open.
///
/// Bytes only. A signed URL is for showing a file in a page, so it opens the
/// routes that serve file bytes and nothing else: no listing, no run record, no
/// config, and nothing that writes.
pub(super) fn is_signable(path: &str) -> bool {
    let bytes = path.starts_with("/api/agents/")
        && (path.contains("/blobs/")
            || path.contains("/artifacts/")
            || path.ends_with("/files/raw"));
    bytes || path.starts_with("/api/exports/")
}

/// Mint a full URL path for one byte route.
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
    let mut query = signer.sign(path, now);
    for (name, value) in extra_query {
        query.push('&');
        query.push_str(name);
        query.push('=');
        query.push_str(value);
    }
    format!("{path}?{query}")
}

#[cfg(test)]
#[path = "signed_url_tests.rs"]
mod tests;
