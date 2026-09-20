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

    /// Something this server depends on answered badly: an MCP server that
    /// refused the OAuth handshake, a model endpoint that would not say what it
    /// serves. Nothing here is wrong, and retrying may well work.
    #[error("{0}")]
    Upstream(String),

    /// The request is well formed and the thing it names cannot answer as it
    /// stands: a yolo profiles file on disk that will not parse. Nothing the
    /// caller sends differently fixes it, and nothing about the run store is
    /// missing, so it is neither a bad request nor a miss.
    #[error("{0}")]
    Unprocessable(String),

    /// The window asked for is not in the thing: an offset past the end of a
    /// file. A different window of the same file is fine, which is what tells
    /// this apart from a bad request.
    #[error("{0}")]
    RangeNotSatisfiable(String),

    /// The bytes are not the kind of thing this read returns: a text read of a
    /// file that is not text. The file is there, and fetching it whole through a
    /// byte route works.
    #[error("{0}")]
    UnsupportedMedia(String),

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
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
            Self::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::RangeNotSatisfiable(_) => StatusCode::RANGE_NOT_SATISFIABLE,
            Self::UnsupportedMedia(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
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
            Self::Upstream(_) => "UPSTREAM",
            Self::Unprocessable(_) => "UNPROCESSABLE",
            Self::RangeNotSatisfiable(_) => "RANGE_NOT_SATISFIABLE",
            Self::UnsupportedMedia(_) => "UNSUPPORTED_MEDIA_TYPE",
            Self::Internal(_) => "INTERNAL",
        }
    }

    /// The same failure, with a sentence added.
    ///
    /// The kind is kept: a missing thing stays missing and a refused one stays
    /// refused, because those are different things for a caller to do about.
    pub(crate) fn with_context(self, extra: &str) -> Self {
        let said = format!("{self}. {extra}");
        match self {
            Self::BadRequest(_) => Self::BadRequest(said),
            Self::NotFound(_) => Self::NotFound(said),
            Self::Conflict(_) => Self::Conflict(said),
            Self::Forbidden(_) => Self::Forbidden(said),
            Self::DaemonUnavailable(_) => Self::DaemonUnavailable(said),
            Self::DaemonIncompatible(_) => Self::DaemonIncompatible(said),
            Self::Upstream(_) => Self::Upstream(said),
            Self::Unprocessable(_) => Self::Unprocessable(said),
            Self::RangeNotSatisfiable(_) => Self::RangeNotSatisfiable(said),
            Self::UnsupportedMedia(_) => Self::UnsupportedMedia(said),
            Self::Internal(_) => Self::Internal(said),
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
                ServeError::Upstream("u".into()),
                StatusCode::BAD_GATEWAY,
                "UPSTREAM",
            ),
            (
                ServeError::Unprocessable("p".into()),
                StatusCode::UNPROCESSABLE_ENTITY,
                "UNPROCESSABLE",
            ),
            (
                ServeError::RangeNotSatisfiable("r".into()),
                StatusCode::RANGE_NOT_SATISFIABLE,
                "RANGE_NOT_SATISFIABLE",
            ),
            (
                ServeError::UnsupportedMedia("m".into()),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "UNSUPPORTED_MEDIA_TYPE",
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

    /// A sentence added to a failure keeps its kind: "not found" with more
    /// detail is still not found, and a client branching on the code sees no
    /// change.
    #[test]
    fn added_context_keeps_the_kind() {
        let missing = ServeError::NotFound("Run 'worker-1' not found".to_string())
            .with_context("It is a sub-agent run of 'root', deleted with it");
        assert_eq!(missing.code(), "NOT_FOUND");
        assert_eq!(
            missing.to_string(),
            "Run 'worker-1' not found. It is a sub-agent run of 'root', deleted with it"
        );
        for failure in [
            ServeError::BadRequest("b".into()),
            ServeError::Conflict("c".into()),
            ServeError::Forbidden("f".into()),
            ServeError::DaemonUnavailable("d".into()),
            ServeError::DaemonIncompatible("i".into()),
            ServeError::Upstream("u".into()),
            ServeError::Unprocessable("p".into()),
            ServeError::RangeNotSatisfiable("r".into()),
            ServeError::UnsupportedMedia("m".into()),
            ServeError::Internal("x".into()),
        ] {
            let code = failure.code();
            assert_eq!(failure.with_context("and more").code(), code);
        }
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
