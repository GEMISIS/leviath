//! Tests for the signed byte URLs.
//!
//! What matters here is what a signature does *not* open: another path, another
//! server, or the same path a while later. Each of those is its own test,
//! because each is a way a leaked URL could have been worth more than the one
//! file it was minted for.

use std::sync::Arc;

use super::{URL_TTL_SECS, UrlSigner, is_signable, signed_path};

/// A signature opens the path it was minted for, until it expires.
#[test]
fn a_signature_opens_its_own_path_until_it_expires() {
    let signer = UrlSigner::default();
    let now = 1_788_924_523;
    let query = signer.sign("/api/agents/run-a/blobs/abc", now);

    assert!(signer.verify("/api/agents/run-a/blobs/abc", Some(&query), now));
    // Still good a minute later, and worthless once the window has passed.
    assert!(signer.verify(
        "/api/agents/run-a/blobs/abc",
        Some(&query),
        now + URL_TTL_SECS - 1
    ));
    assert!(!signer.verify(
        "/api/agents/run-a/blobs/abc",
        Some(&query),
        now + URL_TTL_SECS
    ));
    assert!(!signer.verify(
        "/api/agents/run-a/blobs/abc",
        Some(&query),
        now + URL_TTL_SECS + 3_600
    ));
}

/// A signature for one path does not open another.
///
/// The path is inside the signed message for exactly this reason: one run's
/// readable blob must not be a key to another run's files.
#[test]
fn a_signature_does_not_open_another_path() {
    let signer = UrlSigner::default();
    let now = 1_788_924_523;
    let query = signer.sign("/api/agents/run-a/blobs/abc", now);

    assert!(!signer.verify("/api/agents/run-b/blobs/abc", Some(&query), now));
    assert!(!signer.verify("/api/agents/run-a/blobs/def", Some(&query), now));
    assert!(!signer.verify("/api/agents/run-a/files/raw", Some(&query), now));
}

/// Another server's signature is worthless here: the key is per process, and
/// never written down.
#[test]
fn another_servers_signature_does_not_open_this_ones_files() {
    let theirs = UrlSigner::default();
    let ours = UrlSigner::default();
    let now = 1_788_924_523;
    let query = theirs.sign("/api/agents/run-a/blobs/abc", now);

    assert!(theirs.verify("/api/agents/run-a/blobs/abc", Some(&query), now));
    assert!(!ours.verify("/api/agents/run-a/blobs/abc", Some(&query), now));
}

/// A query with half the pair, a wrong expiry, or nothing at all, opens
/// nothing.
#[test]
fn a_malformed_query_opens_nothing() {
    let signer = UrlSigner::default();
    let now = 1_788_924_523;
    let path = "/api/agents/run-a/blobs/abc";
    let good = signer.sign(path, now);
    let signature = good
        .split('&')
        .find_map(|pair| pair.strip_prefix("sig="))
        .expect("a signature");

    assert!(!signer.verify(path, None, now), "no query at all");
    assert!(!signer.verify(path, Some(""), now), "an empty query");
    assert!(
        !signer.verify(path, Some(&format!("sig={signature}")), now),
        "a signature with no expiry"
    );
    assert!(
        !signer.verify(path, Some(&format!("exp={}", now + 60)), now),
        "an expiry with no signature"
    );
    assert!(
        !signer.verify(path, Some("exp=not-a-number&sig=abc"), now),
        "an expiry that is not a number"
    );
    // The expiry is signed, so moving it invalidates the signature rather than
    // extending the URL's life.
    assert!(
        !signer.verify(
            path,
            Some(&format!("exp={}&sig={signature}", now + 86_400)),
            now
        ),
        "an expiry edited to last longer"
    );
}

/// Only the byte routes are signable. A signed URL is for showing a file, and
/// it opens nothing else.
#[test]
fn only_the_byte_routes_are_signable() {
    assert!(is_signable("/api/agents/run-a/blobs/abc"));
    assert!(is_signable("/api/agents/run-a/artifacts/final"));
    assert!(is_signable("/api/agents/run-a/files/raw"));
    assert!(is_signable("/api/exports/job-1"));

    assert!(!is_signable("/api/runs"), "a listing is not bytes");
    assert!(!is_signable("/api/agents/run-a"), "nor a run record");
    assert!(
        !is_signable("/api/agents/run-a/blobs"),
        "nor a blob listing"
    );
    assert!(!is_signable("/api/config"), "nor the config");
    assert!(!is_signable("/api/agents"), "and nothing that writes");
    assert!(!is_signable("/graphql"));
    assert!(!is_signable("/ws"));
}

/// A minted URL is a path plus the grant, with whatever else the route needs.
///
/// Relative on purpose: it keeps the host, scheme and port the caller already
/// reached, which a server guessing its own public URL would get wrong behind a
/// proxy.
#[test]
fn a_minted_url_is_a_relative_path_with_its_grant() {
    let signer = Arc::new(UrlSigner::default());
    let now = 1_788_924_523;
    let url = signed_path(
        &signer,
        "/api/agents/run-a/files/raw",
        &[("path", "out.png"), ("download", "1")],
        now,
    );

    assert!(url.starts_with("/api/agents/run-a/files/raw?"), "{url}");
    assert!(url.contains("path=out.png"), "{url}");
    assert!(url.contains("download=1"), "{url}");
    // And it verifies, which is the point: the extra parameters are outside the
    // signature, so the route reads them and the grant still holds.
    let (path, query) = url.split_once('?').expect("a query");
    assert!(signer.verify(path, Some(query), now));
}
