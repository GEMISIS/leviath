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

use std::path::Path;

use leviath_core::mime::{Delivery, InboundPart, MimeType};

use super::error::ServeError;

/// A file inside `workdir` as a part.
///
/// Refused when the path escapes the working directory, when the file is
/// missing, when it is empty, or when it is over `max_bytes`. Each is a
/// different thing for the caller to do about, so each is its own failure.
pub(crate) fn read_within(
    path: &str,
    workdir: &Path,
    max_bytes: u64,
) -> Result<InboundPart, ServeError> {
    let full = workdir.join(path);
    if !leviath_core::resolves_within(&full, workdir) {
        return Err(ServeError::Forbidden(format!(
            "part path '{path}' is outside the run's working directory"
        )));
    }
    let data = std::fs::read(&full)
        .map_err(|e| ServeError::BadRequest(format!("part '{path}' could not be read: {e}")))?;
    if data.is_empty() {
        return Err(ServeError::BadRequest(format!("part '{path}' is empty")));
    }
    if data.len() as u64 > max_bytes {
        return Err(ServeError::PayloadTooLarge(format!(
            "part '{path}' is {} bytes, over the {max_bytes} byte ceiling",
            data.len()
        )));
    }
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
