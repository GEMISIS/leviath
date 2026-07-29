//! PKCE (RFC 7636) and CSRF state generation for the OAuth flow.
//!
//! MCP servers register the client as public (no secret), so PKCE is what
//! actually binds the token request to the browser session that authorized it.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use sha2::{Digest, Sha256};

/// A PKCE verifier/challenge pair plus the CSRF `state` for one authorization.
#[derive(Debug, Clone)]
pub(crate) struct Pkce {
    /// The high-entropy secret, sent only on the token request.
    pub(crate) verifier: String,
    /// `BASE64URL(SHA256(verifier))`, sent on the authorization request.
    pub(crate) challenge: String,
    /// Opaque value echoed through the redirect to detect a forged callback.
    pub(crate) state: String,
}

impl Pkce {
    /// Generate a fresh pair from the system CSPRNG.
    pub(crate) fn generate() -> Self {
        // 32 random bytes → 43 base64url chars, comfortably inside the spec's
        // 43–128 range and well above the 256 bits of entropy it wants.
        let verifier = random_token(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_token(16);
        Self {
            verifier,
            challenge,
            state,
        }
    }
}

/// A URL-safe random token of `bytes` bytes of entropy.
fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_is_within_the_spec_length_range() {
        // RFC 7636 §4.1: 43–128 characters. `len` is bound first so the
        // assertion message captures a variable rather than a second call -
        // otherwise that call is a region only ever reached on failure.
        let len = Pkce::generate().verifier.len();
        assert!(
            (43..=128).contains(&len),
            "verifier length {len} out of range"
        );
    }

    #[test]
    fn challenge_is_the_s256_of_the_verifier() {
        let pkce = Pkce::generate();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn tokens_are_url_safe_with_no_padding() {
        // The url-safe predicate, named so its arms can be exercised
        // deterministically. Driving it only over the random tokens below would
        // flake the coverage gate: a 43-char base64url token has a sizable chance
        // of containing no `-` (or no `_`) at all, leaving those comparison arms
        // unevaluated on that run.
        let url_safe = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
        // Cover every arm regardless of what the CSPRNG produced this run: an
        // alphanumeric (first arm), a `-` (second arm), a `_` (third arm), and a
        // rejected byte (all arms false).
        assert!(url_safe(b'A'));
        assert!(url_safe(b'9'));
        assert!(url_safe(b'-'));
        assert!(url_safe(b'_'));
        assert!(!url_safe(b'!'));

        let pkce = Pkce::generate();
        for token in [&pkce.verifier, &pkce.challenge, &pkce.state] {
            assert!(
                token.bytes().all(url_safe),
                "token is not url-safe: {token}"
            );
        }
    }

    #[test]
    fn each_generation_is_unique() {
        // A repeated verifier or state would defeat the point of both.
        let a = Pkce::generate();
        let b = Pkce::generate();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.state, b.state);
        assert_ne!(a.challenge, b.challenge);
    }

    #[test]
    fn random_token_length_scales_with_entropy() {
        // base64 of N bytes is ceil(N/3)*4 chars, minus padding; for 32 bytes
        // that is 43. The exact value matters only in that more bytes → more
        // chars, so a caller can reason about entropy.
        assert!(random_token(48).len() > random_token(16).len());
    }
}
