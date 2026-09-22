//! The page-size check, and what a page costs the complexity budget.

use crate::commands::serve::core::error::ServeError;

/// What one page of a listing costs, for the check made before anything runs.
///
/// A listing field costs its page size times whatever one row of it costs, so
/// `runs(first: 200) { children(first: 200) { id } }` is counted as the forty
/// thousand rows it asks for rather than as the handful of words it is written
/// with. Without this, breadth is free and only nesting is counted, which is
/// the opposite of where the work is.
///
/// A `first` that names no page at all counts as nothing. The page-size check
/// is what refuses those, and it runs inside the resolver with a message that
/// says which cap was missed; a complexity refusal in its place would tell a
/// client its query was too large when the real answer is that `first: 0` is
/// not a page.
pub(crate) fn weight(first: i32, child: usize) -> usize {
    usize::try_from(first).unwrap_or(0).saturating_mul(child)
}

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
