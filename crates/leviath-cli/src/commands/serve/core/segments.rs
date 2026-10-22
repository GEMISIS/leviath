//! A run's segments, and whether it can be continued: what `Run.segments`,
//! `Run.continuable` and `GET /api/runs/{id}/segments` answer.
//!
//! A finished run continued with a new message starts a new segment, with its
//! own spend, answer and artifacts. The run's own fields describe the run as
//! it stands, so its answer is the latest segment's; every earlier segment's
//! answer is kept beside the run file, and read back from there.

use leviath_core::run_meta::{RunMeta, RunStatus};
use leviath_runtime::runfile::Segment;

use super::error::ServeError;
use super::run_file;
use crate::runstate;

/// One segment, with its answer's text when it handed one back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SegmentRead {
    /// The segment as the run file records it.
    pub(crate) segment: Segment,
    /// Its answer, read from the file it was kept in. `None` when it handed
    /// back none, or the file no longer reads.
    pub(crate) answer: Option<String>,
}

/// Every segment of the run `run_id`, oldest first, each with its answer. A
/// run with no run file has none.
pub(crate) fn segments(run_id: &str) -> Result<Vec<SegmentRead>, ServeError> {
    let Some(reader) = run_file::open(run_id)? else {
        return Ok(Vec::new());
    };
    let dir = runstate::run_dir(run_id);
    let segments = leviath_runtime::runfile::segments(&reader).map_err(|e| {
        ServeError::Internal(format!(
            "Run '{run_id}' has a run file that will not read: {e}"
        ))
    })?;
    Ok(segments
        .into_iter()
        .map(|segment| {
            let answer = segment
                .file
                .as_ref()
                .and_then(|file| file.read(&dir).ok())
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
            SegmentRead { segment, answer }
        })
        .collect())
}

/// Whether the run `meta` describes can be continued with a new message: it
/// has finished (complete, failed or cancelled), it is not a sub-agent of
/// another run, and its file can be read back to run, which a run converted
/// from an old release without its blueprint cannot. `None` when its file
/// does not say.
pub(crate) fn continuable(meta: &RunMeta) -> Option<bool> {
    let finished = matches!(
        meta.status,
        RunStatus::Complete | RunStatus::Error | RunStatus::Cancelled
    );
    if !finished || meta.parent_run_id.is_some() {
        return Some(false);
    }
    let spec = run_file::require_spec(&meta.run_id).ok()?;
    Some(spec.placement.parent.is_none() && spec.origin.never_resumes().is_none())
}

#[cfg(test)]
#[path = "segments_tests.rs"]
mod tests;
