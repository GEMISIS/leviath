//! Tests for the signed byte URLs.
//!
//! What matters here is what a signature does *not* open: another path, another
//! server, or the same path a while later. Each of those is its own test,
//! because each is a way a leaked URL could have been worth more than the one
//! file it was minted for.

use std::sync::Arc;

use super::{URL_TTL_SECS, UrlSigner, encode, is_signable, signed_path};

/// A signature opens the path it was minted for, until it expires.
#[test]
fn a_signature_opens_its_own_path_until_it_expires() {
    let signer = UrlSigner::default();
    let now = 1_788_924_523;
    let query = signer.sign("/api/runs/run-a/blobs/abc", "", now);

    assert!(signer.verify("/api/runs/run-a/blobs/abc", Some(&query), now));
    // Still good a minute later, and worthless once the window has passed.
    assert!(signer.verify(
        "/api/runs/run-a/blobs/abc",
        Some(&query),
        now + URL_TTL_SECS - 1
    ));
    assert!(!signer.verify(
        "/api/runs/run-a/blobs/abc",
        Some(&query),
        now + URL_TTL_SECS
    ));
    assert!(!signer.verify(
        "/api/runs/run-a/blobs/abc",
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
    let query = signer.sign("/api/runs/run-a/blobs/abc", "", now);

    assert!(!signer.verify("/api/runs/run-b/blobs/abc", Some(&query), now));
    assert!(!signer.verify("/api/runs/run-a/blobs/def", Some(&query), now));
    assert!(!signer.verify("/api/runs/run-a/files/raw", Some(&query), now));
}

/// Another server's signature is worthless here: the key is per process, and
/// never written down.
#[test]
fn another_servers_signature_does_not_open_this_ones_files() {
    let theirs = UrlSigner::default();
    let ours = UrlSigner::default();
    let now = 1_788_924_523;
    let query = theirs.sign("/api/runs/run-a/blobs/abc", "", now);

    assert!(theirs.verify("/api/runs/run-a/blobs/abc", Some(&query), now));
    assert!(!ours.verify("/api/runs/run-a/blobs/abc", Some(&query), now));
}

/// A query with half the pair, a wrong expiry, or nothing at all, opens
/// nothing.
#[test]
fn a_malformed_query_opens_nothing() {
    let signer = UrlSigner::default();
    let now = 1_788_924_523;
    let path = "/api/runs/run-a/blobs/abc";
    let good = signer.sign(path, "", now);
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
    assert!(is_signable("/api/runs/run-a/blobs/abc"));
    assert!(is_signable("/api/runs/run-a/artifacts/final"));
    assert!(is_signable("/api/runs/run-a/files/raw"));
    assert!(is_signable("/api/exports/job-1"));

    assert!(!is_signable("/api/runs"), "a listing is not bytes");
    assert!(!is_signable("/api/runs/run-a"), "nor a run record");
    assert!(!is_signable("/api/runs/run-a/blobs"), "nor a blob listing");
    assert!(!is_signable("/api/config"), "nor the config");
    assert!(
        !is_signable("/api/runs/run-a/cancel"),
        "and nothing that writes"
    );
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
        "/api/runs/run-a/files/raw",
        &[("path", "out.png"), ("download", "1")],
        now,
    );

    assert!(url.starts_with("/api/runs/run-a/files/raw?"), "{url}");
    assert!(url.contains("path=out.png"), "{url}");
    assert!(url.contains("download=1"), "{url}");
    let (path, query) = url.split_once('?').expect("a query");
    assert!(signer.verify(path, Some(query), now));
}

/// A link to one file in a working directory opens that file and no other.
///
/// `files/raw` names its file in the query, so the query is inside the
/// signature: a link minted for `out.png` with its `path=` edited, or with a
/// second `path=` added, is refused.
#[test]
fn a_file_link_does_not_open_another_file() {
    let signer = Arc::new(UrlSigner::default());
    let now = 1_788_924_523;
    let url = signed_path(
        &signer,
        "/api/runs/run-a/files/raw",
        &[("path", "out.png")],
        now,
    );
    let (path, query) = url.split_once('?').expect("a query");
    assert!(signer.verify(path, Some(query), now));

    let edited = query.replace("path=out.png", "path=.env");
    assert!(!signer.verify(path, Some(&edited), now), "{edited}");
    let doubled = format!("{query}&path=.env");
    assert!(!signer.verify(path, Some(&doubled), now), "{doubled}");
    let dropped = query.replace("&path=out.png", "");
    assert!(!signer.verify(path, Some(&dropped), now), "{dropped}");
}

/// `download` is the one parameter outside the signature. It only decides
/// whether the browser saves the file or shows it, so a console may add it to a
/// link it was handed.
#[test]
fn download_may_be_added_to_a_minted_link() {
    let signer = Arc::new(UrlSigner::default());
    let now = 1_788_924_523;
    let url = signed_path(
        &signer,
        "/api/runs/run-a/files/raw",
        &[("path", "out.png")],
        now,
    );
    let (path, query) = url.split_once('?').expect("a query");
    assert!(signer.verify(path, Some(&format!("{query}&download=1")), now));
    assert!(signer.verify(path, Some(&format!("{query}&download")), now));
    // A name that only starts with it is signed like any other.
    assert!(!signer.verify(path, Some(&format!("{query}&downloads=1")), now));
}

/// A name with a space, `&`, `#`, `+`, `%` or `/` comes back as itself.
///
/// Written into a link as it is, `&` would end the name and start another
/// parameter, `#` would end the URL, and `+` would read back as a space.
#[test]
fn odd_names_survive_the_link() {
    let signer = Arc::new(UrlSigner::default());
    let now = 1_788_924_523;
    for name in [
        "my report.pdf",
        "a&b=c.txt",
        "notes#2.md",
        "c++.txt",
        "100%.txt",
        "out/plot.png",
        "été.txt",
    ] {
        let url = signed_path(&signer, "/api/runs/run-a/files/raw", &[("path", name)], now);
        let (path, query) = url.split_once('?').expect("a query");
        assert!(!query.contains('#'), "{url}");
        assert!(signer.verify(path, Some(query), now), "{url}");
        // Decoded the way the route's query extractor decodes it.
        let read = url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "path")
            .map(|(_, value)| value.into_owned());
        assert_eq!(read.as_deref(), Some(name), "{url}");
    }
}

/// [`encode`] leaves the unreserved characters and encodes the rest, so an
/// artifact name is one path segment however it is spelled.
#[test]
fn encode_keeps_a_name_to_one_segment() {
    assert_eq!(encode("final-v1.2_x~"), "final-v1.2_x~");
    assert_eq!(encode("my plot"), "my%20plot");
    assert_eq!(encode("a/b"), "a%2Fb");
    assert_eq!(encode("x?y#z"), "x%3Fy%23z");
}
