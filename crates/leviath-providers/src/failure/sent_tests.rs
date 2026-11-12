//! Tests for [`super`]: what each error says about whether its request went
//! out, by variant, by kind, and by what a real socket reports.

use super::*;
use crate::provider::{UnavailableReason, build_http_client};
use std::io::{Read, Write};
use std::time::Duration;

/// A loopback server on a plain thread, handing each connection to `serve`.
fn serve(serve: impl Fn(std::net::TcpStream) + Send + 'static) -> std::net::SocketAddr {
    let listener = leviath_testkit::bind_local_std();
    let addr = listener.local_addr().expect("has an address");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            serve(stream);
        }
    });
    addr
}

/// Read one request off `stream`, up to the end of its headers.
fn read_request(stream: &mut std::net::TcpStream) {
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = stream.read(&mut buf).expect("the request arrives");
        seen.extend_from_slice(&buf[..read]);
    }
}

/// The failure of a GET to `url` through the provider client, within `secs`.
async fn failure(url: String, secs: u64) -> reqwest::Error {
    build_http_client(Some(secs))
        .expect("a client builds")
        .get(url)
        .send()
        .await
        .expect_err("the request fails")
}

/// Every variant, untagged: the ones that end before a request exists say
/// not sent, the ones built from an answer say sent, and only `Other` and an
/// unlabelled transport failure cannot say.
#[test]
fn every_variant_says_whether_its_request_went_out() {
    let guard = ProviderError::TokenLimitExceeded {
        used: 9,
        reply_budget: 1,
        max: 8,
        sent: false,
    };
    let answered = ProviderError::TokenLimitExceeded {
        used: 9,
        reply_budget: 1,
        max: 8,
        sent: true,
    };
    for (error, sent) in [
        (ProviderError::ClientBuild("no roots".into()), Some(false)),
        (ProviderError::RetentionRefused("kept".into()), Some(false)),
        (guard, Some(false)),
        (answered, Some(true)),
        (
            ProviderError::RateLimitExceeded {
                retry_after_secs: None,
            },
            Some(true),
        ),
        (
            ProviderError::Unavailable {
                reason: UnavailableReason::AuthFailed,
                detail: "HTTP 401: no".into(),
            },
            Some(true),
        ),
        (ProviderError::ApiError("HTTP 400: no".into()), Some(true)),
        (
            ProviderError::InvalidResponse("not json".into()),
            Some(true),
        ),
        (ProviderError::RequestFailed("it broke".into()), None),
        (ProviderError::Other("mystery".into()), None),
    ] {
        assert_eq!(error.request_sent(), sent, "{error}");
    }
}

/// Every kind, on the transport error that carries it. Stated exhaustively:
/// the three that end before a connection, the five that are answers, and
/// the three that happen on either side of the send and so say nothing.
#[test]
fn every_kind_says_what_it_settles() {
    for (kind, sent) in [
        (FailureKind::DnsFailure, Some(false)),
        (FailureKind::ConnectionRefused, Some(false)),
        (FailureKind::TlsFailure, Some(false)),
        (FailureKind::Timeout, None),
        (FailureKind::ConnectionDropped, None),
        (FailureKind::Transport, None),
        (FailureKind::BadRequest, Some(true)),
        (FailureKind::NotFound, Some(true)),
        (FailureKind::ServerError, Some(true)),
        (FailureKind::ModelUnavailable, Some(true)),
        (FailureKind::MalformedResponse, Some(true)),
        (FailureKind::WrongEndpoint, Some(true)),
    ] {
        let label = kind.label();
        assert_eq!(kind.request_sent(), sent, "{label}");
        let error = ProviderError::labelled(kind, "sending the request", "it failed");
        assert_eq!(error.request_sent(), sent, "{label}");
    }
    assert_eq!(FailureKind::ALL.len(), 12, "a new kind needs a row above");
}

/// A tag at the head of the message answers before the variant or the kind
/// does, after the kind's label or on its own, and survives the kind being
/// read back.
#[test]
fn a_tag_answers_before_the_variant_and_the_kind_do() {
    let timeout = |tag: &str| ProviderError::RequestFailed(format!("[timeout] {tag}connecting"));
    assert_eq!(timeout("").request_sent(), None);
    assert_eq!(timeout("[not-sent] ").request_sent(), Some(false));
    assert_eq!(timeout("[sent] ").request_sent(), Some(true));
    assert_eq!(
        timeout("[sent] ").failure_kind(),
        Some(FailureKind::Timeout)
    );
    // A tag ahead of the label is read the same way.
    let ahead = ProviderError::RequestFailed("[sent] [timeout] waiting".into());
    assert_eq!(ahead.request_sent(), Some(true));
    assert_eq!(ahead.failure_kind(), Some(FailureKind::Timeout));
    // "Unknown" overrides a kind that would answer.
    let refused = ProviderError::RequestFailed("[connection-refused] [sent-unknown] x".into());
    assert_eq!(refused.request_sent(), None);
    // An answer that says it was never sent, and an `Other` that says it was.
    let unsent = ProviderError::ApiError("[not-sent] 'x' is not a mode".into());
    assert_eq!(unsent.request_sent(), Some(false));
    let sent = ProviderError::Other("[sent] the job failed".into());
    assert_eq!(sent.request_sent(), Some(true));
}

/// Only lowercase words in brackets are tags: a provider's own bracketed
/// text, an empty pair and a bracket that never closes are the message.
#[test]
fn only_bracketed_words_at_the_head_are_tags() {
    assert_eq!(
        leading_tags("[timeout] [sent] waiting"),
        (vec!["timeout", "sent"], "waiting")
    );
    assert_eq!(leading_tags("[timeout]"), (vec!["timeout"], ""));
    assert_eq!(leading_tags("plain"), (vec![], "plain"));
    for text in [
        "[1, 2] is not a tag",
        "[] nor is this",
        "[Not Sent] nor this",
        "[never closed",
    ] {
        assert_eq!(leading_tags(text), (vec![], text));
        assert_eq!(ProviderError::Other(text.into()).request_sent(), None);
    }
    // The tags stop at the first thing that is not one.
    assert_eq!(leading_tags("[sent] [1] rest"), (vec!["sent"], "[1] rest"));
}

/// Saying a request went out tags the message once, after the kind's label,
/// and changes nothing on an error that already says so.
#[test]
fn after_send_tags_only_what_does_not_already_say_so() {
    let refused = ProviderError::labelled(FailureKind::ConnectionRefused, "polling", "no");
    assert_eq!(refused.request_sent(), Some(false));
    let later = refused.after_send();
    assert_eq!(later.request_sent(), Some(true));
    assert_eq!(later.failure_kind(), Some(FailureKind::ConnectionRefused));
    let text = later.to_string();
    assert!(
        text.starts_with("Request failed: [connection-refused] [sent] polling: no"),
        "{text}"
    );
    // Said twice, it is written once.
    assert_eq!(later.after_send().to_string(), text);

    // An HTTP error is an answer already, so its message is left alone.
    let api = ProviderError::ApiError("[bad-request] HTTP 400: no".into()).after_send();
    assert_eq!(api.to_string(), "API error: [bad-request] HTTP 400: no");

    let job = ProviderError::Other("the job failed".into()).after_send();
    assert_eq!(job.to_string(), "[sent] the job failed");
    // An earlier answer is replaced, not stacked.
    let flipped = job.before_send();
    assert_eq!(flipped.to_string(), "[not-sent] the job failed");
    assert_eq!(flipped.request_sent(), Some(false));
}

/// A refusal made before sending says so on every variant that can be one,
/// and `unsent` is the plain way to build it.
#[test]
fn before_send_marks_a_refusal_made_on_this_machine() {
    let refusal = ProviderError::unsent("the stage handed it no text");
    assert!(
        matches!(&refusal, ProviderError::Other(m) if m == "[not-sent] the stage handed it no text")
    );
    assert_eq!(refusal.request_sent(), Some(false));
    assert!(!refusal.is_transient());
    assert_eq!(refusal.failure_kind(), None);

    let retired = ProviderError::Unavailable {
        reason: UnavailableReason::ModelRetired,
        detail: "retired on 2026-01-01".into(),
    }
    .before_send();
    assert_eq!(retired.request_sent(), Some(false));
    assert_eq!(
        retired.unavailable_reason(),
        Some(UnavailableReason::ModelRetired)
    );
    // Already not sent: nothing to add.
    let build = ProviderError::ClientBuild("no roots".into()).before_send();
    assert!(matches!(&build, ProviderError::ClientBuild(m) if m == "no roots"));
    // And each of them can be told a request had gone out after all.
    for error in [
        ProviderError::ClientBuild("x".into()),
        ProviderError::RetentionRefused("x".into()),
        ProviderError::InvalidResponse("[not-sent] x".into()),
        ProviderError::RequestFailed("x".into()),
    ] {
        assert_eq!(error.after_send().request_sent(), Some(true));
    }
}

/// The two variants with no message: a token limit holds its own yes or no,
/// and a rate limit is only ever a provider's answer.
#[test]
fn a_token_limit_holds_its_answer_and_a_rate_limit_keeps_its_own() {
    let guard = ProviderError::TokenLimitExceeded {
        used: 9,
        reply_budget: 1,
        max: 8,
        sent: false,
    };
    let later = guard.after_send();
    assert_eq!(later.request_sent(), Some(true));
    // It has no way to say "unknown", so it keeps what it held.
    let kept = later.with_request_sent(None);
    assert_eq!(kept.request_sent(), Some(true));
    assert_eq!(kept.before_send().request_sent(), Some(false));

    let limited = ProviderError::RateLimitExceeded {
        retry_after_secs: Some(3),
    };
    let limited = limited.before_send().with_request_sent(None);
    assert_eq!(limited.request_sent(), Some(true));
    assert!(matches!(
        limited,
        ProviderError::RateLimitExceeded {
            retry_after_secs: Some(3)
        }
    ));
}

/// The tag for "cannot say" is written where the variant would otherwise
/// answer, and an HTTP status behind tags is still read.
#[test]
fn an_unknown_is_written_and_tags_do_not_hide_a_status() {
    let unknown = ProviderError::ApiError("HTTP 402: pay".into()).with_request_sent(None);
    assert_eq!(
        unknown.to_string(),
        "API error: [sent-unknown] HTTP 402: pay"
    );
    assert_eq!(unknown.request_sent(), None);
    assert_eq!(
        UnavailableReason::from_message("[server-error] [sent-unknown] HTTP 402: pay"),
        Some(UnavailableReason::CreditsExhausted)
    );
    assert_eq!(
        tagged(Some(true), "[timeout] [not-sent] x"),
        "[timeout] [sent] x"
    );
    assert_eq!(said("[timeout] x"), None);
    assert_eq!(said("[timeout] [sent-unknown] x"), Some(None));
}

/// Several requests in one call: sent once any of them was, not sent only
/// when none was.
#[test]
fn a_call_of_several_requests_is_sent_once_any_was() {
    for (earlier, last, both) in [
        (Some(false), Some(false), Some(false)),
        (Some(false), Some(true), Some(true)),
        (Some(true), Some(false), Some(true)),
        (None, Some(true), Some(true)),
        (Some(true), None, Some(true)),
        (Some(false), None, None),
        (None, Some(false), None),
        (None, None, None),
    ] {
        assert_eq!(either(earlier, last), both, "{earlier:?} then {last:?}");
    }
}

/// The placing itself, including the arm no socket produces on demand: a
/// send that failed with nothing to say which side of the connection it was
/// on stays unknown.
#[test]
fn a_failure_with_no_stage_in_it_is_not_guessed_at() {
    assert_eq!(placed(true, false, ""), Some(false));
    assert_eq!(placed(false, true, ""), Some(true));
    assert_eq!(
        placed(
            false,
            false,
            "error sending request | client error (sendrequest) | reset"
        ),
        Some(true)
    );
    assert_eq!(
        placed(false, false, "error sending request | operation timed out"),
        None
    );
}

/// A resolver that fails every lookup, or never answers one. Both are
/// connect-stage failures that need no network and read the same on every
/// platform, which a closed port does not (Windows drops the SYN where Unix
/// refuses).
struct Resolver {
    answers: bool,
}

impl reqwest::dns::Resolve for Resolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let answers = self.answers;
        Box::pin(async move {
            if !answers {
                std::future::pending::<()>().await;
            }
            let message = format!("failed to lookup address information: {}", name.as_str());
            Err(std::io::Error::other(message).into())
        })
    }
}

/// The failure of a GET through a client resolving with `resolver`, whose
/// connect may take `connect` of a 20 second deadline.
async fn connect_failure_with(resolver: Resolver, connect: Duration) -> reqwest::Error {
    reqwest::Client::builder()
        .dns_resolver(resolver)
        .connect_timeout(connect)
        .timeout(Duration::from_secs(20))
        .build()
        .expect("a client builds")
        .get("https://no-such-host-anywhere-12345.invalid/v1/models")
        .send()
        .await
        .expect_err("the connect fails")
}

/// A name that does not resolve: the request never left, and the kind
/// already says so, so the message carries no tag.
#[tokio::test]
async fn a_name_that_does_not_resolve_was_not_sent() {
    let e = connect_failure_with(Resolver { answers: true }, Duration::from_secs(10)).await;
    assert_eq!(reqwest_request_sent(&e), Some(false));
    let error = ProviderError::transport("sending the request", &e);
    assert_eq!(error.failure_kind(), Some(FailureKind::DnsFailure));
    assert_eq!(error.request_sent(), Some(false));
    assert!(!error.to_string().contains("[not-sent]"), "{error}");
}

/// A request the client refuses to build never left either. Its kind is the
/// unplaced `transport`, which alone cannot say, so the tag does.
#[tokio::test]
async fn a_request_that_could_not_be_built_was_not_sent() {
    let e = failure("ftp://example.invalid/models".to_string(), 2).await;
    assert!(e.is_builder());
    let error = ProviderError::transport("sending the request", &e);
    assert_eq!(error.failure_kind(), Some(FailureKind::Transport));
    assert_eq!(error.request_sent(), Some(false));
    assert!(
        error
            .to_string()
            .starts_with("Request failed: [transport] [not-sent] sending the request"),
        "{error}"
    );
}

/// A connect that cannot finish in time is a timeout by kind and was never
/// sent. `reqwest` calls this `is_request()` too, as it does every failed
/// send, so that flag says nothing about whether the request went out.
#[tokio::test]
async fn a_connect_that_times_out_was_not_sent() {
    let e = connect_failure_with(Resolver { answers: false }, Duration::from_millis(300)).await;
    assert!(e.is_request() && e.is_connect() && e.is_timeout());
    let error = ProviderError::transport("sending the request", &e);
    assert_eq!(error.failure_kind(), Some(FailureKind::Timeout));
    assert_eq!(error.request_sent(), Some(false));
    assert!(
        error.to_string().contains("[timeout] [not-sent]"),
        "{error}"
    );
}

/// A provider that takes the request and never answers: the deadline fires
/// with nothing in the error to say the connection was up, which is the one
/// transport failure that cannot be placed.
#[tokio::test]
async fn a_deadline_with_no_stage_in_it_cannot_say() {
    let addr = serve(|stream| {
        std::thread::sleep(Duration::from_secs(3));
        drop(stream);
    });
    let e = failure(format!("http://{addr}/v1/models"), 1).await;
    assert!(e.is_timeout() && !e.is_connect());
    let error = ProviderError::transport("sending the request", &e);
    assert_eq!(error.failure_kind(), Some(FailureKind::Timeout));
    assert_eq!(error.request_sent(), None);
}

/// The peer read the request and closed without answering. `reqwest` says
/// `is_request()` and nothing else, and the request demonstrably went out.
#[tokio::test]
async fn a_connection_closed_after_the_request_was_read_was_sent() {
    let addr = serve(|mut stream| read_request(&mut stream));
    let e = failure(format!("http://{addr}/v1/models"), 5).await;
    assert!(e.is_request() && !e.is_connect() && !e.is_timeout());
    let error = ProviderError::transport("sending the request", &e);
    assert_eq!(error.failure_kind(), Some(FailureKind::Transport));
    assert_eq!(error.request_sent(), Some(true));
    assert!(error.to_string().contains("[transport] [sent]"), "{error}");
}

/// An answer that is not HTTP is still an answer to a request that went out.
#[tokio::test]
async fn an_answer_that_is_not_http_was_sent() {
    let addr = serve(|mut stream| {
        read_request(&mut stream);
        let _ = stream.write_all(b"NOT HTTP AT ALL\r\n\r\n");
    });
    let e = failure(format!("http://{addr}/v1/models"), 5).await;
    let error = ProviderError::transport("sending the request", &e);
    assert_eq!(error.request_sent(), Some(true));
}

/// A body that stops short arrives after the headers did: sent, and tagged,
/// since a dropped connection alone does not say which side it dropped on.
#[tokio::test]
async fn a_body_cut_short_was_sent() {
    let base = leviath_testkit::spawn_mock_server_truncated_body(200, "OK").await;
    let response = build_http_client(Some(5))
        .expect("a client builds")
        .get(&base)
        .send()
        .await
        .expect("the headers arrive");
    let e = response.bytes().await.expect_err("the body is cut");
    assert_eq!(reqwest_request_sent(&e), Some(true));
    let error = ProviderError::transport("reading the response body", &e);
    assert_eq!(error.failure_kind(), Some(FailureKind::ConnectionDropped));
    assert_eq!(error.request_sent(), Some(true));
    assert!(
        error.to_string().contains("[connection-dropped] [sent]"),
        "{error}"
    );
}

/// A redirect that cannot be followed is an answer too.
#[tokio::test]
async fn a_redirect_that_fails_was_sent() {
    let addr = serve(|mut stream| {
        read_request(&mut stream);
        let _ = stream.write_all(
            b"HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
    });
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(1))
        .timeout(Duration::from_secs(5))
        .build()
        .expect("a client builds");
    let e = client
        .get(format!("http://{addr}/first"))
        .send()
        .await
        .expect_err("the redirects run out");
    assert!(e.is_redirect());
    assert_eq!(reqwest_request_sent(&e), Some(true));
}

/// The status the provider answered with, where the error is such an answer.
#[test]
fn an_error_says_the_status_the_provider_answered_with() {
    use crate::provider::UnavailableReason;
    let rate_limited = ProviderError::RateLimitExceeded {
        retry_after_secs: None,
    };
    assert_eq!(rate_limited.answered_status(), Some(429));
    assert!(rate_limited.refused_unrun());

    let overflowed = |sent| ProviderError::TokenLimitExceeded {
        used: 1,
        reply_budget: 1,
        max: 1,
        sent,
    };
    assert_eq!(overflowed(true).answered_status(), Some(400));
    assert_eq!(overflowed(false).answered_status(), None);
    assert!(!overflowed(true).refused_unrun(), "a 400 leaves it open");

    for (status, unrun) in [
        (401, true),
        (402, true),
        (404, true),
        (400, false),
        (500, false),
    ] {
        let error = ProviderError::ApiError(format!("HTTP {status} something"));
        assert_eq!(error.answered_status(), Some(status));
        assert_eq!(error.refused_unrun(), unrun, "{status}");
    }
    let out_of_credits = ProviderError::Unavailable {
        reason: UnavailableReason::CreditsExhausted,
        detail: "HTTP 402 Payment Required".into(),
    };
    assert_eq!(out_of_credits.answered_status(), Some(402));

    // An error that came inside a reply already under way names no status,
    // and neither does anything that is not the provider's answer.
    for error in [
        ProviderError::ApiError("the model provider reported: overloaded".into()),
        ProviderError::RequestFailed("[timeout] reading: slow".into()),
        ProviderError::ClientBuild("no certs".into()),
        ProviderError::InvalidResponse("not json".into()),
        ProviderError::RetentionRefused("zdr".into()),
        ProviderError::Other("x".into()),
    ] {
        assert_eq!(error.answered_status(), None, "{error}");
        assert!(!error.refused_unrun());
    }
}
