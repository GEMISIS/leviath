//! Files a request names by path inside a run's working directory.
//!
//! Both surfaces let a caller attach a file it does not want to upload: REST
//! takes a JSON `parts` list, GraphQL an `attachments` list on the spawn and
//! message requests. Either way the path is resolved inside the run's working
//! directory and refused when it escapes, so the reading itself, and every
//! refusal it can answer with, is described once here.
//!
//! The declared `mime_type` and `deliver` words are read here too, for the same
//! reason: a caller that spells `deliver` wrong should be told the same thing on
//! both surfaces.

use std::cell::Cell;
use std::io::Read as _;
use std::path::Path;

use leviath_core::mime::{Delivery, InboundPart, MimeType};

use super::error::ServeError;

/// How many bytes the files one request names by path may come to between
/// them. An upload is held to the server's body limit as it arrives; this
/// holds the files a request names to the same figure, each to what the
/// ones before it left.
#[derive(Debug)]
pub(crate) struct FileBudget {
    left: Cell<u64>,
}

impl FileBudget {
    /// A budget of `bytes`.
    pub(crate) fn new(bytes: u64) -> Self {
        Self {
            left: Cell::new(bytes),
        }
    }
}

/// A file inside `workdir` as a part, counted against `budget`.
///
/// Refused when the path escapes the working directory, when it is missing
/// or not a regular file, when it is empty, or when it is over what `budget`
/// has left. Each is a different thing for the caller to do about, so each is
/// its own failure. No more than one byte past the budget is ever read, so a
/// huge file is refused without being read whole.
pub(crate) fn read_within(
    path: &str,
    workdir: &Path,
    budget: &FileBudget,
) -> Result<InboundPart, ServeError> {
    let full = workdir.join(path);
    if !leviath_core::resolves_within(&full, workdir) {
        return Err(ServeError::Forbidden(format!(
            "part path '{path}' is outside the run's working directory"
        )));
    }
    let unreadable =
        |e: std::io::Error| ServeError::BadRequest(format!("part '{path}' could not be read: {e}"));
    let (file, meta) = std::fs::File::open(&full)
        .and_then(|file| file.metadata().map(|meta| (file, meta)))
        .map_err(unreadable)?;
    if !meta.is_file() {
        return Err(ServeError::BadRequest(format!(
            "part '{path}' is not a file"
        )));
    }
    let left = budget.left.get();
    let mut data = Vec::new();
    file.take(left.saturating_add(1))
        .read_to_end(&mut data)
        .map_err(unreadable)?;
    let size = (data.len() as u64).max(meta.len());
    if size > left {
        return Err(ServeError::PayloadTooLarge(format!(
            "part '{path}' is {size} bytes, over the {left} bytes this request's files may \
             still come to"
        )));
    }
    if data.is_empty() {
        return Err(ServeError::BadRequest(format!("part '{path}' is empty")));
    }
    budget.left.set(left - size);
    let name = full
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(InboundPart::from_bytes(name, data))
}

/// The `deliver` word a request used, as the runtime's choice.
pub(crate) fn delivery(word: &str) -> Result<Delivery, ServeError> {
    Delivery::from_arg(word).map_err(ServeError::BadRequest)
}

/// The type a request declared for the part it named at `path`.
pub(crate) fn mime_type(path: &str, declared: &str) -> Result<MimeType, ServeError> {
    MimeType::parse(declared).map_err(|e| {
        ServeError::BadRequest(format!(
            "part '{path}' has mime_type '{declared}', which is not one: {e}"
        ))
    })
}

#[cfg(test)]
#[path = "attachments_tests.rs"]
mod tests;
