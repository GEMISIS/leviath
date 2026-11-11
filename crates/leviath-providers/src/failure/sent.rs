//! Whether a failed call's request went out.
//!
//! A caller that keeps its own account of spend has one question about a call
//! that failed: could the provider have been charged for it. A request that
//! never left this machine cost nothing; one that left may have, whatever
//! came back. [`ProviderError::request_sent`] answers that, and this module is
//! where the answer is worked out and carried.
//!
//! It is carried the way a [`FailureKind`] is: as a bracketed tag at the head
//! of the message (`[sent]`, `[not-sent]`, `[sent-unknown]`), after the kind's
//! own label when there is one. The message is the one part of an error that
//! survives a Rhai script and every layer that passes strings, and a tag is
//! only written where the variant and the kind do not already settle it.

use super::{FailureKind, error_chain_text};
use crate::provider::ProviderError;

/// The tag for a call whose request went out.
const SENT: &str = "sent";
/// The tag for a call that put nothing on the wire.
const NOT_SENT: &str = "not-sent";
/// The tag for a call that cannot say, on an error whose variant or kind would
/// otherwise answer for it.
const SENT_UNKNOWN: &str = "sent-unknown";

/// The tag that says `sent`.
fn tag_of(sent: Option<bool>) -> &'static str {
    match sent {
        Some(true) => SENT,
        Some(false) => NOT_SENT,
        None => SENT_UNKNOWN,
    }
}

/// What a tag says about the request, when it is one of the three that do.
fn said_by(tag: &str) -> Option<Option<bool>> {
    match tag {
        SENT => Some(Some(true)),
        NOT_SENT => Some(Some(false)),
        SENT_UNKNOWN => Some(None),
        _ => None,
    }
}

/// The bracketed tags `message` opens with, and the text after them.
///
/// A tag is `[word]` where the word is lowercase letters and hyphens: a
/// [`FailureKind`]'s label, or one of the three that say whether a request
/// went out. Anything else in brackets (a JSON array a provider answered
/// with, a bracket that never closes) is the message's own text and ends the
/// tags.
pub(crate) fn leading_tags(message: &str) -> (Vec<&str>, &str) {
    let mut tags = Vec::new();
    let mut rest = message;
    while let Some((tag, after)) = rest.strip_prefix('[').and_then(|r| r.split_once(']')) {
        let word = !tag.is_empty() && tag.bytes().all(|b| b.is_ascii_lowercase() || b == b'-');
        if !word {
            break;
        }
        tags.push(tag);
        rest = after.strip_prefix(' ').unwrap_or(after);
    }
    (tags, rest)
}

/// `message` with `sent` said at its head: after the tags it already opens
/// with, in place of any earlier answer.
pub(crate) fn tagged(sent: Option<bool>, message: &str) -> String {
    let (tags, rest) = leading_tags(message);
    let mut out = String::with_capacity(message.len() + 16);
    for tag in tags.into_iter().filter(|tag| said_by(tag).is_none()) {
        out.push_str(&format!("[{tag}] "));
    }
    out.push_str(&format!("[{}] {rest}", tag_of(sent)));
    out
}

/// What `message` says about the request, when it carries a tag that says.
pub(crate) fn said(message: &str) -> Option<Option<bool>> {
    leading_tags(message).0.into_iter().find_map(said_by)
}

/// The answer for a call that made a request after `earlier` ones, the last
/// of which ended `last`: sent when any of them was, not sent when none was,
/// and unknown otherwise.
pub(crate) fn either(earlier: Option<bool>, last: Option<bool>) -> Option<bool> {
    match (earlier, last) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

/// Whether the request behind a `reqwest` failure went out.
///
/// Measured against real sockets rather than read off the method names,
/// because `is_request()` is true for every failure of a send, a refused
/// connection and a reply that never came alike:
///
/// - a failure to build the request, or to connect (a name that did not
///   resolve, a refusal, a handshake that failed, a connect that timed out),
///   happened before there was a connection to write to: not sent;
/// - a failure reading the body, or following a redirect, happened after an
///   answer began to arrive: sent;
/// - a failure the HTTP client reports from sending on an open connection
///   (the peer closed it, reset it, or answered something that is not HTTP):
///   sent, in whole or in part;
/// - a deadline that fired with neither mark cannot be placed. The error for a
///   connect still under way when the deadline passed is the same error as
///   the one for a provider that took the request and never answered.
pub(crate) fn reqwest_request_sent(e: &reqwest::Error) -> Option<bool> {
    placed(
        e.is_builder() || e.is_connect(),
        e.is_body() || e.is_decode() || e.is_redirect(),
        &error_chain_text(e),
    )
}

/// [`reqwest_request_sent`], over what the error says about itself: whether
/// it failed before a connection existed, whether an answer had begun, and
/// its cause chain in lowercase.
///
/// A free function so the last arm is reachable from a test, as
/// [`super::connect_failure`]'s is: a send failure with no stage in it comes
/// from a real socket only by racing a deadline.
fn placed(before_connecting: bool, answered: bool, chain: &str) -> Option<bool> {
    if before_connecting {
        return Some(false);
    }
    // The HTTP client names the stage a send failed at, and this is the one
    // that comes after the connection was made.
    if answered || chain.contains("client error (sendrequest)") {
        return Some(true);
    }
    None
}

impl FailureKind {
    /// Whether a request went out, where the kind alone settles it.
    ///
    /// A name that did not resolve, a refusal and a failed handshake all end
    /// before there is a connection to send on. An HTTP status and an answer
    /// that would not parse are both answers. A timeout, a dropped connection
    /// and an unplaced transport failure happen on either side of the send, so
    /// the kind says nothing and the error's own tag does (see
    /// [`ProviderError::request_sent`]).
    pub fn request_sent(self) -> Option<bool> {
        match self {
            FailureKind::DnsFailure | FailureKind::ConnectionRefused | FailureKind::TlsFailure => {
                Some(false)
            }
            FailureKind::BadRequest
            | FailureKind::NotFound
            | FailureKind::ServerError
            | FailureKind::ModelUnavailable
            | FailureKind::MalformedResponse => Some(true),
            FailureKind::Timeout | FailureKind::ConnectionDropped | FailureKind::Transport => None,
        }
    }
}

impl ProviderError {
    /// Whether anything this call asked of the provider went out before it
    /// failed.
    ///
    /// - `Some(false)`: nothing did. The call ended on this machine: refused
    ///   before sending, or with no connection to send on. The provider never
    ///   heard of it and cannot have charged for it.
    /// - `Some(true)`: a connection was open and a request was given to it,
    ///   so the provider may have received it, in whole or in part, and may
    ///   have acted on it. A
    ///   call that makes several requests (a job submitted and then polled, a
    ///   request sent again without a refused parameter) says this once any
    ///   of them has gone out, even when the one that failed never connected.
    /// - `None`: the error cannot say. A deadline that fired with nothing to
    ///   show whether the connection was up, an error a provider script threw
    ///   without the host knowing what it had sent, or a failure raised above
    ///   the provider (the dispatch layer's own timeout).
    ///
    /// A tag at the head of the message answers when there is one. Otherwise
    /// the variant does: a client that was never built, a retention refusal
    /// and a window guard's [`ProviderError::TokenLimitExceeded`] are not
    /// sent; a rate limit, an HTTP error, an unusable provider and an answer
    /// that would not parse are; a transport failure is whatever its
    /// [`FailureKind`] settles; and [`ProviderError::Other`] is unknown.
    pub fn request_sent(&self) -> Option<bool> {
        match self.message().and_then(said) {
            Some(said) => said,
            None => self.request_sent_untagged(),
        }
    }

    /// [`Self::request_sent`] for an error whose message carries no tag.
    fn request_sent_untagged(&self) -> Option<bool> {
        match self {
            ProviderError::ClientBuild(_) | ProviderError::RetentionRefused(_) => Some(false),
            ProviderError::TokenLimitExceeded { sent, .. } => Some(*sent),
            ProviderError::RateLimitExceeded { .. }
            | ProviderError::Unavailable { .. }
            | ProviderError::ApiError(_)
            | ProviderError::InvalidResponse(_) => Some(true),
            ProviderError::RequestFailed(_) => {
                self.failure_kind().and_then(FailureKind::request_sent)
            }
            ProviderError::Other(_) => None,
        }
    }

    /// The text this error's tags ride in: its message, or an unusable
    /// provider's detail. A rate limit and a token limit have none.
    fn message(&self) -> Option<&str> {
        match self {
            ProviderError::RequestFailed(m)
            | ProviderError::ApiError(m)
            | ProviderError::ClientBuild(m)
            | ProviderError::InvalidResponse(m)
            | ProviderError::RetentionRefused(m)
            | ProviderError::Other(m)
            | ProviderError::Unavailable { detail: m, .. } => Some(m),
            ProviderError::RateLimitExceeded { .. } | ProviderError::TokenLimitExceeded { .. } => {
                None
            }
        }
    }

    /// This error, saying a request of its call had gone out by the time it
    /// was raised.
    ///
    /// For the step of a call that comes after another: a poll or a download
    /// behind a submitted job, a second attempt behind an answered first. The
    /// step that failed may never have connected, and the provider still has
    /// what went before it.
    pub fn after_send(self) -> Self {
        self.with_request_sent(Some(true))
    }

    /// This error, saying nothing was sent: the call was refused on this
    /// machine before any request was made.
    pub fn before_send(self) -> Self {
        self.with_request_sent(Some(false))
    }

    /// A call refused on this machine before anything was sent, for a reason
    /// no other variant names: an input the stage did not supply, a parameter
    /// the provider cannot take, an operation it does not have.
    pub fn unsent(message: impl Into<String>) -> Self {
        ProviderError::Other(message.into()).before_send()
    }

    /// This error with `sent` as its answer to [`Self::request_sent`].
    ///
    /// The message is tagged only when the error does not already say so. A
    /// rate limit is left as it is: it has no message to tag, and it is only
    /// ever a provider's answer. A token limit holds a yes or a no, so an
    /// unknown leaves it alone too.
    pub(crate) fn with_request_sent(mut self, sent: Option<bool>) -> Self {
        if self.request_sent() == sent {
            return self;
        }
        match &mut self {
            ProviderError::RequestFailed(m)
            | ProviderError::ApiError(m)
            | ProviderError::ClientBuild(m)
            | ProviderError::InvalidResponse(m)
            | ProviderError::RetentionRefused(m)
            | ProviderError::Other(m)
            | ProviderError::Unavailable { detail: m, .. } => *m = tagged(sent, m),
            ProviderError::TokenLimitExceeded { sent: held, .. } => {
                *held = sent.unwrap_or(*held);
            }
            ProviderError::RateLimitExceeded { .. } => {}
        }
        self
    }
}

#[cfg(test)]
#[path = "sent_tests.rs"]
mod tests;
