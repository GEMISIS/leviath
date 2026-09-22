//! The page-size check, written once.

use crate::commands::serve::core::error::ServeError;

/// Check a requested page size against a listing's cap.
///
/// Refused rather than clamped. REST clamps because a query string is often
/// hand-written and a clamped answer is still useful; a GraphQL client builds
/// its query in code, and silently getting 200 of the 500 it asked for is the
/// kind of bug that only shows up as missing rows much later.
///
/// `what` names the cap in the refusal, because the caps differ by listing and
/// "at most 50" without saying which 50 is not something a client author can
/// act on. It reads as the tail of a sentence: `the executions page cap`.
pub(crate) fn page(first: i32, cap: usize, what: &str) -> Result<usize, ServeError> {
    match usize::try_from(first) {
        // A negative `first`, and zero, are the same mistake: neither names a
        // page, and both are what a client sends when it meant to leave the
        // argument out.
        Ok(0) | Err(_) => Err(ServeError::BadRequest(
            "`first` must be at least 1; omit it for the default".to_string(),
        )),
        Ok(n) if n > cap => Err(ServeError::BadRequest(format!(
            "`first` may be at most {cap}, {what}"
        ))),
        Ok(n) => Ok(n),
    }
}

#[cfg(test)]
#[path = "page_tests.rs"]
mod tests;
