//! What a run actually did, one page at a time.
//!
//! The journal is the only record of this, and it is read rather than folded:
//! the point is what happened, including the attempts that failed, were refused
//! or were cut off, which a folded window no longer shows.
//!
//! The payloads stay out of the page on purpose. A page of executions is the
//! facts about each attempt, and a result is fetched for the one execution
//! somebody opened, by the position that page reported. Otherwise a request for
//! "what did this run do" would read every file body the run ever produced.

use leviath_core::run_archive::Execution;

use super::error::ServeError;
use crate::runstate;

/// Default page size.
pub(crate) const EXECUTIONS_DEFAULT_LIMIT: usize = 50;

/// Largest page of executions.
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

/// Which page of a run's executions to read.
#[derive(Debug)]
pub(crate) struct ExecutionsSpec {
    /// How many to return.
    pub(crate) limit: usize,
    /// How many to skip, from the previous page's cursor.
    pub(crate) after: Option<usize>,
    /// The digest the cursor was minted against.
    pub(crate) digest: String,
}

impl ExecutionsSpec {
    /// Read the request's own words into a spec, refusing what cannot be
    /// answered.
    pub(crate) fn resolve(
        run_id: &str,
        limit: Option<usize>,
        cursor: Option<&str>,
    ) -> Result<Self, ServeError> {
        let limit = match limit {
            None => EXECUTIONS_DEFAULT_LIMIT,
            Some(0) => {
                return Err(ServeError::BadRequest(
                    "`limit` must be at least 1; omit it for the default".to_string(),
                ));
            }
            Some(n) => n.min(EXECUTIONS_MAX_LIMIT),
        };
        let digest = super::super::cursor::filter_digest(&[run_id]);
        let after = match cursor {
            None => None,
            Some(raw) => {
                let decoded = super::super::cursor::decode(raw, "index", "asc", &digest)
                    .map_err(|e| ServeError::BadRequest(e.message()))?;
                match decoded.key {
                    super::super::cursor::CursorKey::Int(i) => usize::try_from(i).ok(),
                    // This listing mints only an integer key, so anything else
                    // is a cursor from somewhere else.
                    _ => None,
                }
            }
        };
        Ok(Self {
            limit,
            after,
            digest,
        })
    }
}

/// One page of a run's executions.
#[derive(Debug)]
pub(crate) struct ExecutionsPage {
    /// The executions themselves, in dispatch order.
    pub(crate) executions: Vec<Execution>,
    /// Where the next page starts. Nothing when this page reached the end.
    pub(crate) next_cursor: Option<String>,
    /// How many the journal holds altogether.
    pub(crate) total: usize,
}

/// Read one page of what a run did.
///
/// Dispatch order, always. It is the order the run happened in, it is the order
/// the journal holds, and a debugger reading a batch wants its calls together
/// rather than interleaved by whichever finished first.
pub(crate) fn page(run_id: &str, spec: &ExecutionsSpec) -> Result<ExecutionsPage, ServeError> {
    let executions = read(run_id)?;
    let total = executions.len();
    let start = spec.after.map(|i| i + 1).unwrap_or(0);
    let mut wanted: Vec<(usize, Execution)> = executions
        .into_iter()
        .enumerate()
        .skip(start)
        .take(spec.limit + 1)
        .collect();
    let has_more = wanted.len() > spec.limit;
    wanted.truncate(spec.limit);
    let next_cursor = has_more.then(|| wanted.last()).flatten().map(|(index, _)| {
        super::super::cursor::encode(
            "index",
            "asc",
            &spec.digest,
            super::super::cursor::CursorKey::Int(*index as i64),
            "",
        )
    });
    Ok(ExecutionsPage {
        executions: wanted.into_iter().map(|(_, e)| e).collect(),
        next_cursor,
        total,
    })
}

/// Every execution a run's journal records.
///
/// A run with no journal is not an error here: a run that never dispatched a tool
/// did nothing, and an empty list says so. A run id that names nothing is the
/// caller's problem to catch, which it does by reading the run first.
fn read(run_id: &str) -> Result<Vec<Execution>, ServeError> {
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
