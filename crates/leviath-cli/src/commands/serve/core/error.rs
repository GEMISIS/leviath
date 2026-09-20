//! The one failure type the service layer returns.
//!
//! Before this existed, every handler built its own `(StatusCode, Json)`
//! pair, so the same failure could answer 404 on one route and 500 on
//! another, and GraphQL would have had to re-derive all of it from status
//! codes. A failure is now described once, by what went wrong, and each
//! surface renders it: REST as a status and `{"error": ...}` body, GraphQL
//! as an `errors` entry carrying a machine-readable `code`.

use axum::http::StatusCode;

/// What went wrong, described by cause rather than by status code.
///
/// The variants are deliberately few. Each one has a different remedy for
/// whoever reads it, which is the test for whether a new variant earns its
/// place.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ServeError {
    /// The request itself is wrong: an unknown field name, a bad cursor, a
    /// page size over the cap, two answer variants at once. Retrying it
    /// unchanged fails the same way.
    #[error("{0}")]
    BadRequest(String),

    /// Nothing by that name, or nothing in the state the request needs: an
    /// unknown run id, an interaction that was already answered.
    #[error("{0}")]
    NotFound(String),

    /// The thing exists, and its current state refuses the change. A
    /// terminal run cannot be paused, and a stale digest pin cannot spawn.
    #[error("{0}")]
    Conflict(String),

    /// The server is configured to refuse this: a workdir outside
    /// `--workdir-root`, an unattended run on a `--no-remote-yolo` server, a
    /// callback URL the outbound policy will not allow.
    #[error("{0}")]
    Forbidden(String),

    /// The daemon could not be reached. It may be restarting (the control
    /// client already waited out its grace period), stopped, or wedged.
    #[error("Daemon not reachable: {0}")]
    DaemonUnavailable(String),

    /// The daemon answered, but this server cannot understand the answer:
    /// the daemon was updated under a running `lev serve`. Retrying cannot
    /// help, so the message names what does.
    #[error("This server needs a restart: {0}")]
    DaemonIncompatible(String),

    /// Something failed that the caller did nothing wrong to cause: a file
    /// this server wrote will not parse, a reply with no arm for it.
    #[error("{0}")]
    Internal(String),
}

impl ServeError {
    /// The HTTP status this failure answers with.
    ///
    /// GraphQL reports the same number in `extensions.httpStatus`, so a
    /// client that already knows the REST vocabulary reads either surface
    /// without a second table.
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::DaemonUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::DaemonIncompatible(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The machine-readable code GraphQL puts in `extensions.code`.
    ///
    /// Stable vocabulary: a client switches on this, never on the message,
    /// which is written for a person and may be reworded.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "BAD_USER_INPUT",
            Self::NotFound(_) => "NOT_FOUND",
            Self::Conflict(_) => "CONFLICT",
            Self::Forbidden(_) => "FORBIDDEN",
            Self::DaemonUnavailable(_) => "DAEMON_UNAVAILABLE",
            Self::DaemonIncompatible(_) => "DAEMON_INCOMPATIBLE",
            Self::Internal(_) => "INTERNAL",
        }
    }

    /// The failure for a daemon that did not answer.
    ///
    /// The error's kind tells the two apart: `Unsupported` is the control
    /// client's way of saying the protocol versions no longer match, and
    /// everything else means the socket itself did not work.
    pub(crate) fn from_daemon_io(e: &std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::Unsupported => Self::DaemonIncompatible(e.to_string()),
            _ => Self::DaemonUnavailable(e.to_string()),
        }
    }

    /// A daemon reply this call has no arm for.
    ///
    /// Internal rather than a gateway failure: the reply decoded, so the two
    /// processes still speak the same protocol. This server simply asked one
    /// question and was handed the answer to another.
    pub(crate) fn unexpected_reply(
        other: &leviath_runtime::control_socket::ControlResponse,
    ) -> Self {
        Self::Internal(format!("Unexpected daemon response: {other:?}"))
    }
}

/// Render a service failure as the REST surface's `(status, JSON)` pair.
///
/// A free function rather than a `From` impl because [`ApiError`] is a tuple
/// alias, and a tuple of foreign types cannot carry one. The body shape is
/// unchanged from before this module existed: `{"error": "..."}`.
///
/// [`ApiError`]: super::super::types::ApiError
pub(crate) fn as_api_error(e: &ServeError) -> super::super::types::ApiError {
    super::super::types::err(e.status(), e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{ServeError, as_api_error};
    use axum::http::StatusCode;

    /// Every variant's status and code, in one table: the mapping is the
    /// contract both surfaces render, so it is checked as a whole rather
    /// than a case at a time.
    #[test]
    fn each_variant_answers_its_own_status_and_code() {
        let cases = [
            (
                ServeError::BadRequest("b".into()),
                StatusCode::BAD_REQUEST,
                "BAD_USER_INPUT",
            ),
            (
                ServeError::NotFound("n".into()),
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
            ),
            (
                ServeError::Conflict("c".into()),
                StatusCode::CONFLICT,
                "CONFLICT",
            ),
            (
                ServeError::Forbidden("f".into()),
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
            ),
            (
                ServeError::DaemonUnavailable("d".into()),
                StatusCode::SERVICE_UNAVAILABLE,
                "DAEMON_UNAVAILABLE",
            ),
            (
                ServeError::DaemonIncompatible("d".into()),
                StatusCode::BAD_GATEWAY,
                "DAEMON_INCOMPATIBLE",
            ),
            (
                ServeError::Internal("i".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL",
            ),
        ];
        for (error, status, code) in cases {
            assert_eq!(error.status(), status, "status for {error:?}");
            assert_eq!(error.code(), code, "code for {error:?}");
        }
    }

    /// The message a person reads is the one the variant carries, with the
    /// daemon cases naming the remedy rather than only the failure.
    #[test]
    fn messages_read_as_written() {
        assert_eq!(ServeError::NotFound("no run".into()).to_string(), "no run");
        assert_eq!(
            ServeError::DaemonUnavailable("socket closed".into()).to_string(),
            "Daemon not reachable: socket closed"
        );
        assert_eq!(
            ServeError::DaemonIncompatible("v2 frame".into()).to_string(),
            "This server needs a restart: v2 frame"
        );
    }

    /// A protocol mismatch is the one io failure with a different remedy, so
    /// it is the one that maps somewhere else.
    #[test]
    fn a_protocol_mismatch_is_told_apart_from_an_unreachable_socket() {
        // Compared by code rather than by `matches!`: a `matches!` inside an
        // assert leaves the non-matching arm as a region nothing reaches, and
        // the code is the thing a client branches on anyway.
        let unsupported = std::io::Error::new(std::io::ErrorKind::Unsupported, "daemon speaks v2");
        assert_eq!(
            ServeError::from_daemon_io(&unsupported).code(),
            "DAEMON_INCOMPATIBLE"
        );
        let broken = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone");
        assert_eq!(
            ServeError::from_daemon_io(&broken).code(),
            "DAEMON_UNAVAILABLE"
        );
    }

    /// A daemon that is not there is a 503: try later, or restart it. A daemon
    /// that answered in a way this server cannot read is a 502 with the remedy
    /// in the message, because no daemon restart fixes that one.
    #[test]
    fn a_daemon_failure_says_which_remedy_applies() {
        let (code, body) = as_api_error(&ServeError::from_daemon_io(&std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "no socket",
        )));
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.0.error, "Daemon not reachable: no socket");

        let (code, body) = as_api_error(&ServeError::from_daemon_io(&std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the daemon is now version 9; restart this process",
        )));
        assert_eq!(code, StatusCode::BAD_GATEWAY);
        assert_eq!(
            body.0.error,
            "This server needs a restart: the daemon is now version 9; restart this process"
        );
    }

    /// The REST body keeps the shape every client already parses, and the
    /// status is the variant's own.
    #[test]
    fn the_rest_rendering_is_the_status_and_the_error_body() {
        let (status, body) = as_api_error(&ServeError::Conflict("run is finished".into()));
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.0.error, "run is finished");
    }

    /// An answer to a question this call did not ask says so, and names the
    /// reply so the daemon log and the response agree.
    #[test]
    fn an_unexpected_reply_names_what_came_back() {
        let reply = leviath_runtime::control_socket::ControlResponse::Ok { ok: true };
        let error = ServeError::unexpected_reply(&reply);
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(error.to_string().contains("Ok"), "names the reply: {error}");
    }
}
