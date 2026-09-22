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

use std::sync::Arc;

use async_graphql::extensions::{
    Extension, ExtensionContext, ExtensionFactory, NextParseQuery, NextValidation,
};
use async_graphql::parser::types::ExecutableDocument;
use async_graphql::{
    Error, ErrorExtensions, ServerError, ServerResult, ValidationResult, Variables,
};

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

/// The code every refusal reached before a resolver runs carries.
///
/// A query that will not parse, names a field that is not there, spells an
/// enum wrong, fills two members of a `@oneOf` input, or asks for more nesting
/// or more breadth than the limits allow, is one thing to a client: the
/// document it sent is wrong, and sending it again will not help. That is what
/// REST answers 400 to, so it is what this answers `BAD_USER_INPUT` to.
const BAD_REQUEST_CODE: &str = "BAD_USER_INPUT";

/// The status REST answers the same refusal with.
const BAD_REQUEST_STATUS: u16 = 400;

/// Stamp one pre-execution refusal with the code a client branches on.
///
/// The message the parser or the validator wrote is left exactly as it is: it
/// names the line and column, which is the part a person reads.
fn coded(mut error: ServerError) -> ServerError {
    let extensions = error.extensions.get_or_insert_with(Default::default);
    extensions.set("code", BAD_REQUEST_CODE);
    extensions.set("httpStatus", BAD_REQUEST_STATUS);
    error
}

/// Registers [`CodedRefusals`] on the schema.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CodeEveryRefusal;

impl ExtensionFactory for CodeEveryRefusal {
    fn create(&self) -> Arc<dyn Extension> {
        Arc::new(CodedRefusals)
    }
}

/// Gives the parser's and the validator's refusals the same `extensions.code`
/// a resolver's failure carries.
///
/// A resolver reaches [`graphql_error`] and comes back coded. Nothing a query
/// is refused *before* a resolver runs goes through a resolver, so without
/// this those refusals arrive with `extensions: null` and a client is left
/// matching on the message text, which is the one thing the docs tell it never
/// to do.
#[derive(Debug, Clone, Copy)]
struct CodedRefusals;

#[async_trait::async_trait]
impl Extension for CodedRefusals {
    async fn parse_query(
        &self,
        ctx: &ExtensionContext<'_>,
        query: &str,
        variables: &Variables,
        next: NextParseQuery<'_>,
    ) -> ServerResult<ExecutableDocument> {
        next.run(ctx, query, variables).await.map_err(coded)
    }

    async fn validation(
        &self,
        ctx: &ExtensionContext<'_>,
        next: NextValidation<'_>,
    ) -> Result<ValidationResult, Vec<ServerError>> {
        next.run(ctx)
            .await
            .map_err(|refusals| refusals.into_iter().map(coded).collect())
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
