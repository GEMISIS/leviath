use super::*;

/// A fetcher that answers one canned body, and records what it was asked for.
fn answering(body: &'static str) -> ReleaseFetcher {
    Arc::new(move |_: &str| Ok(body.to_string()))
}

/// The three channels read three different tags. Stable reads `latest` rather
/// than a version tag, because the version is the thing being looked up.
#[test]
fn each_channel_asks_for_its_own_tag() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = {
        let seen = Arc::clone(&seen);
        Arc::new(move |url: &str| {
            seen.lock().expect("not poisoned").push(url.to_string());
            Err("no network".to_string())
        }) as ReleaseFetcher
    };
    for channel in [Channel::Stable, Channel::Beta, Channel::Alpha] {
        check_with(channel, "0.4.0", &recorder, 1);
    }
    let asked = seen.lock().expect("not poisoned").clone();
    assert!(asked[0].ends_with("/latest"), "stable: {}", asked[0]);
    assert!(asked[1].ends_with("/beta"), "beta: {}", asked[1]);
    assert!(asked[2].ends_with("/alpha"), "alpha: {}", asked[2]);
}

/// The alpha and beta tags are the words `alpha` and `beta`, so the tag is not a
/// version and only the release name carries one.
#[test]
fn a_channel_tag_that_is_not_a_version_reads_the_release_name() {
    let body = r#"{"tag_name": "alpha", "name": "Leviath 0.5.0-alpha.3"}"#;
    let got = check_with(Channel::Alpha, "0.4.0", &answering(body), 99);
    assert_eq!(got.latest.as_deref(), Some("0.5.0-alpha.3"));
    assert_eq!(got.update_available, Some(true));
    assert_eq!(got.checked_at, Some(99));
}

/// A `v` prefix and a title around the number are both things a release name
/// carries in practice.
#[test]
fn a_version_is_read_out_of_the_text_around_it() {
    for (name, want) in [
        (r#"{"name": "v0.4.2"}"#, "0.4.2"),
        (r#"{"name": "Leviath 0.4.2"}"#, "0.4.2"),
        (r#"{"tag_name": "0.4.2"}"#, "0.4.2"),
    ] {
        let got = check_with(Channel::Stable, "0.4.0", &answering(name), 1);
        assert_eq!(got.latest.as_deref(), Some(want), "from {name}");
    }
}

/// The comparison is numeric per field. A string compare puts `0.10.0` before
/// `0.9.0`, which would tell everyone on the newer build to downgrade.
#[test]
fn versions_compare_as_numbers_not_as_text() {
    let cases = [
        ("0.10.0", "0.9.0", true),
        ("0.9.0", "0.10.0", false),
        ("0.10.0", "0.10.0", false),
        ("0.4.2", "0.4.0", true),
        ("1.0.0", "0.99.99", true),
    ];
    for (latest, running, want) in cases {
        let body = format!(r#"{{"name": "{latest}"}}"#);
        let fetch: ReleaseFetcher = {
            let body = body.clone();
            Arc::new(move |_: &str| Ok(body.clone()))
        };
        let got = check_with(Channel::Stable, running, &fetch, 1);
        assert_eq!(
            got.update_available,
            Some(want),
            "latest {latest} against running {running}"
        );
    }
}

/// The release a pre-release was building towards is an update over it, and a
/// pre-release of the same number is not an update over the release.
#[test]
fn a_release_beats_the_pre_release_of_the_same_number() {
    let released = check_with(
        Channel::Alpha,
        "0.5.0-alpha.1",
        &answering(r#"{"name": "0.5.0"}"#),
        1,
    );
    assert_eq!(released.update_available, Some(true), "0.5.0 over an alpha");

    let backwards = check_with(
        Channel::Alpha,
        "0.5.0",
        &answering(r#"{"name": "0.5.0-alpha.9"}"#),
        1,
    );
    assert_eq!(
        backwards.update_available,
        Some(false),
        "an alpha is not an update over the release"
    );
}

/// No network, and a body that is not a release, are the same answer: nothing to
/// show. A console renders that as "can't tell", which is what it already does.
#[test]
fn an_unanswerable_check_reports_nothing_rather_than_a_guess() {
    let offline: ReleaseFetcher = Arc::new(|_: &str| Err("dns".to_string()));
    assert_eq!(
        check_with(Channel::Stable, "0.4.0", &offline, 1),
        LatestCheck::default()
    );

    for junk in [r#"{"message": "Not Found"}"#, "not json at all", "{}"] {
        let got = check_with(Channel::Stable, "0.4.0", &answering_owned(junk), 1);
        assert_eq!(got, LatestCheck::default(), "from {junk}");
        assert!(
            got.checked_at.is_none(),
            "an answerless check is not a fresh answer"
        );
    }
}

fn answering_owned(body: &str) -> ReleaseFetcher {
    let body = body.to_string();
    Arc::new(move |_: &str| Ok(body.clone()))
}

/// A cache with no answer in it is stale, which is what makes the first request
/// start a refresh instead of waiting for a timer.
#[test]
fn an_unchecked_cache_is_stale_and_a_fresh_one_is_not() {
    assert!(LatestCheck::default().is_stale(1000, CHECK_TTL_SECS));

    let fresh = LatestCheck {
        checked_at: Some(1000),
        ..LatestCheck::default()
    };
    assert!(!fresh.is_stale(1000 + CHECK_TTL_SECS - 1, CHECK_TTL_SECS));
    assert!(fresh.is_stale(1000 + CHECK_TTL_SECS, CHECK_TTL_SECS));
    assert!(
        !fresh.is_stale(500, CHECK_TTL_SECS),
        "a clock that went backwards must not make an answer stale"
    );
}

/// The stamp is a real time, not a placeholder, so a console can say how fresh
/// the answer is.
#[test]
fn now_secs_is_a_plausible_wall_clock() {
    // Any clock later than the day this was written. Asserting a range rather
    // than a value keeps the test from depending on when it runs.
    assert!(
        now_secs() > 1_700_000_000,
        "unix seconds, not milliseconds or zero"
    );
}

/// The real fetcher, against a server that answers, so the client it builds and
/// the body it reads are exercised rather than only described.
///
/// A plain `#[test]`: the fetcher blocks, and blocking inside a tokio runtime
/// panics. The daemon calls it from `spawn_blocking` for the same reason.
#[test]
fn the_real_fetcher_reads_a_body_off_the_wire() {
    use std::io::{Read, Write};

    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port is available");
    let addr = listener
        .local_addr()
        .expect("a bound listener has an address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("the fetcher connects");
        // Read just enough to let the client finish sending before answering.
        let mut buf = [0_u8; 1024];
        let _ = socket.read(&mut buf);
        let body = r#"{"name": "1.2.3"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes());
    });

    let got = fetch_release(&format!("http://{addr}/releases/tags/latest"));
    server.join().expect("the server thread does not panic");
    assert_eq!(got.as_deref(), Ok(r#"{"name": "1.2.3"}"#));
}

/// A loopback server that answers one request with `body` and the
/// `Content-Length` to match, so the fetcher's read is exercised end to end.
fn serve_once(body: &'static str) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    serve_once_declaring(body, body.len())
}

/// [`serve_once`] with the `Content-Length` chosen by the test, so a server
/// that promises more than it sends can be stood up.
fn serve_once_declaring(
    body: &'static str,
    declared: usize,
) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};

    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port is available");
    let addr = listener
        .local_addr()
        .expect("a bound listener has an address");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("the fetcher connects");
        // Read just enough to let the client finish sending before answering.
        let mut buf = [0_u8; 1024];
        let _ = socket.read(&mut buf);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\nContent-Type: application/json\r\n\r\n{body}"
        );
        let _ = socket.write_all(response.as_bytes());
    });
    (addr, server)
}

/// A connection that closes before the promised body arrives is an error
/// from the read itself, reported like any other transport failure.
#[test]
fn the_real_fetcher_reports_a_body_cut_short_as_an_error() {
    let body = r#"{"name": "1.2.3"}"#;
    let (addr, server) = serve_once_declaring(body, body.len() + 100);
    let got = fetch_release_capped(&format!("http://{addr}/releases/tags/latest"), 1024);
    server.join().expect("the server thread does not panic");
    assert!(got.is_err());
}

/// A body past the cap is an error naming the cap and the peer, in the shape
/// every other capped read reports, rather than a body read whole. The cap
/// is a test-sized one so the test allocates bytes, not 64 MiB.
#[test]
fn the_real_fetcher_stops_at_the_cap_and_names_it() {
    let body = r#"{"name": "1.2.3", "padding": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}"#;
    let (addr, server) = serve_once(body);
    let got = fetch_release_capped(&format!("http://{addr}/releases/tags/latest"), 16);
    server.join().expect("the server thread does not panic");
    assert_eq!(
        got,
        Err("response body exceeded 16 bytes from 127.0.0.1".to_string())
    );
}

/// A body exactly at the cap is whole: the cap is a ceiling, not a strict bound.
#[test]
fn the_real_fetcher_reads_a_body_exactly_at_the_cap() {
    let body = r#"{"name": "1.2.3"}"#;
    let (addr, server) = serve_once(body);
    let got = fetch_release_capped(&format!("http://{addr}/releases/tags/latest"), body.len());
    server.join().expect("the server thread does not panic");
    assert_eq!(got.as_deref(), Ok(body));
}

/// A server that is not there is an error, not a hang and not a panic. The
/// caller turns it into "cannot tell".
#[test]
fn the_real_fetcher_reports_a_refused_connection_as_an_error() {
    // Port 1 needs privileges to bind, so nothing of ours is listening on it.
    assert!(fetch_release("http://127.0.0.1:1/releases/tags/latest").is_err());
}
