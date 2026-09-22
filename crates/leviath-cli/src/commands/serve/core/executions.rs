//! What a run actually did, read whole.
//!
//! The journal is the only record of this, and it is read rather than folded:
//! the point is what happened, including the attempts that failed, were refused
//! or were cut off, which a folded window no longer shows.
//!
//! The payloads stay out of the listing on purpose. Every execution is a
//! handful of facts plus the arguments the model sent, and a result is fetched
//! for the one execution somebody opened, by the position the listing reported.
//! Otherwise a request for "what did this run do" would read every file body
//! the run ever produced.

use leviath_core::run_archive::Execution;

use super::error::ServeError;
use crate::runstate;

/// Largest page of executions the GraphQL listing takes.
///
/// The same cap as the run listing: an execution is a handful of fields plus the
/// arguments the model sent, which is the same order of size as a run's summary.
pub(crate) const EXECUTIONS_MAX_LIMIT: usize = 200;

/// Most bytes of one result to serve inline.
///
/// A result can be a whole file, and a whole file belongs behind a byte route
/// rather than inside a JSON answer. What is served is the head, because the head
/// is what a person reads first and what a failure usually says.
pub(crate) const RESULT_MAX_BYTES: usize = 64 * 1024;

/// Every execution a run's journal records, in dispatch order.
///
/// A run with no journal is not an error here: a run that never dispatched a tool
/// did nothing, and an empty list says so. A run id that names nothing is the
/// caller's problem to catch, which it does by reading the run first.
pub(crate) fn read(run_id: &str) -> Result<Vec<Execution>, ServeError> {
    let path = runstate::run_dir(run_id).join(leviath_core::files::ARCHIVE_FILE);
    let Ok(file) = std::fs::File::open(&path) else {
        return Ok(Vec::new());
    };
    let mut reader = std::io::BufReader::new(file);
    leviath_core::run_archive::read_archive_executions(&mut reader)
        .map_err(|e| ServeError::Internal(format!("Run '{run_id}' has an unreadable journal: {e}")))
}

/// One execution's result, as far as it fits.
///
/// The position comes from the page that listed the execution, and a position
/// that names no result answers `None` rather than erroring: a journal a caller
/// read a moment ago can have been deleted since.
pub(crate) fn result(
    run_id: &str,
    position: u64,
    call_id: &str,
) -> Result<Option<ResultText>, ServeError> {
    let path = runstate::run_dir(run_id).join(leviath_core::files::ARCHIVE_FILE);
    let Ok(mut file) = std::fs::File::open(&path) else {
        return Ok(None);
    };
    let found = leviath_core::run_archive::read_result_at(&mut file, position, call_id)
        .map_err(|e| {
            ServeError::Internal(format!("Run '{run_id}' has an unreadable journal: {e}"))
        })?
        .map(|content| {
            let text = content.as_str();
            ResultText {
                bytes: text.len(),
                text: leviath_core::text::truncate_at_boundary(text, RESULT_MAX_BYTES).to_string(),
                // The stored parts by name, where they have one. A part with no
                // name is referenced by its hash, which the run's own parts
                // listing carries.
                parts: content
                    .stored()
                    .filter_map(|part| part.name.clone())
                    .collect(),
            }
        });
    Ok(found)
}

/// One execution's result, cut to what is reasonable to send.
#[derive(Debug)]
pub(crate) struct ResultText {
    /// The text, up to the cap.
    pub(crate) text: String,
    /// How many bytes the whole result is, which is larger than `text` when it
    /// was cut.
    pub(crate) bytes: usize,
    /// The stored parts the result carried, by name. The bytes themselves are
    /// fetched from the run's parts, where they already live.
    pub(crate) parts: Vec<String>,
}

impl ResultText {
    /// Whether the text is only the head of the result.
    pub(crate) fn truncated(&self) -> bool {
        self.bytes > self.text.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{read, result};

    /// A run with no journal recorded no executions, and an empty list says so
    /// rather than an error.
    #[test]
    fn a_run_with_no_journal_did_nothing() {
        crate::runstate::with_isolated_runs_dir("executions-read-empty", |_dir| {
            assert!(
                read("no-such-run")
                    .expect("no journal is not a failure")
                    .is_empty()
            );
        });
    }

    /// A journal that cannot be read is reported rather than read as empty.
    #[test]
    fn an_unreadable_journal_is_reported() {
        crate::runstate::with_isolated_runs_dir("executions-read-corrupt", |_dir| {
            let dir = crate::runstate::run_dir("broken");
            std::fs::create_dir_all(&dir).expect("a run dir");
            std::fs::write(
                dir.join(leviath_core::files::ARCHIVE_FILE),
                b"not an archive",
            )
            .expect("a corrupt journal");
            let failed = read("broken").expect_err("an unreadable journal");
            assert_eq!(failed.code(), "INTERNAL");
        });
    }

    /// A result asked for on a run with no journal answers nothing.
    ///
    /// Not an error: the caller read a page a moment ago and the run has been
    /// deleted since, which is a race rather than a mistake.
    #[test]
    fn a_result_from_a_run_with_no_journal_is_nothing() {
        crate::runstate::with_isolated_runs_dir("executions-no-journal", |_dir| {
            assert!(
                result("no-such-run", 0, "c1")
                    .expect("no journal is not a failure")
                    .is_none()
            );
        });
    }

    /// A journal that cannot be read is reported rather than read as empty.
    #[test]
    fn a_result_from_an_unreadable_journal_is_an_error() {
        crate::runstate::with_isolated_runs_dir("executions-corrupt", |_dir| {
            let dir = crate::runstate::run_dir("broken");
            std::fs::create_dir_all(&dir).expect("a run dir");
            std::fs::write(
                dir.join(leviath_core::files::ARCHIVE_FILE),
                b"not an archive",
            )
            .expect("a corrupt journal");
            let failed = result("broken", 0, "c1").expect_err("an unreadable journal");
            assert_eq!(failed.code(), "INTERNAL");
            assert!(
                failed.to_string().contains("unreadable journal"),
                "{failed}"
            );
        });
    }
}
