//! How a service failure reaches a GraphQL client.
//!
//! REST puts the failure in the status line. GraphQL answers 200 and carries
//! failures in an `errors` array, each entry naming the `path` in the query
//! that produced it. That is the shape that makes partial answers possible: a
//! page of fifty runs where one run's stage ledger will not parse returns the
//! other forty-nine and one error pointing at the field that failed.
//!
//! So the machine-readable part has to live in `extensions`. A client
//! switches on `code`; `httpStatus` is the same number REST would have
//! answered, for a client that already knows that vocabulary.

use async_graphql::{Error, ErrorExtensions};

use super::super::core::error::ServeError;

/// Render a service failure as a GraphQL error.
///
/// The message is the one a person reads, unchanged from REST, and the code
/// is the stable thing to branch on.
pub(crate) fn graphql_error(e: &ServeError) -> Error {
    let code = e.code();
    let status = e.status().as_u16();
    Error::new(e.to_string()).extend_with(|_, ext| {
        ext.set("code", code);
        ext.set("httpStatus", status);
    })
}

/// Let a resolver write `core_call()?` and get the right error shape.
///
/// A resolver returns `async_graphql::Result`, so without this every call
/// site would repeat the same `map_err`, and the one that forgot would answer
/// with an uncoded error a client cannot branch on.
pub(crate) trait IntoGraphql<T> {
    /// Convert a service result into a GraphQL result.
    fn gql(self) -> async_graphql::Result<T>;
}

impl<T> IntoGraphql<T> for Result<T, ServeError> {
    fn gql(self) -> async_graphql::Result<T> {
        self.map_err(|e| graphql_error(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::{IntoGraphql, graphql_error};
    use crate::commands::serve::core::error::ServeError;

    /// The code and the status travel together: a client that branches on
    /// either reads the same failure.
    #[test]
    fn a_failure_carries_its_code_and_its_rest_status() {
        let error = graphql_error(&ServeError::NotFound("no run 'ghost'".into()));
        assert_eq!(error.message, "no run 'ghost'");
        let ext = error.extensions.expect("extensions");
        assert_eq!(
            ext.get("code").map(ToString::to_string),
            Some("\"NOT_FOUND\"".to_string())
        );
        assert_eq!(
            ext.get("httpStatus").map(ToString::to_string),
            Some("404".to_string())
        );
    }

    /// The happy path passes the value through untouched.
    #[test]
    fn a_value_passes_through() {
        let ok: Result<i32, ServeError> = Ok(7);
        assert_eq!(ok.gql().expect("kept the value"), 7);
    }

    /// A failure converts, so a resolver only writes `?`.
    #[test]
    fn a_failure_converts_at_the_question_mark() {
        let failed: Result<i32, ServeError> = Err(ServeError::Conflict("finished".into()));
        let error = failed.gql().expect_err("stayed a failure");
        assert_eq!(error.message, "finished");
    }
}
