//! Naming a blueprint or a region on the write side, and the one shape a
//! key/value pair takes there.
//!
//! A domain entity is not a bare string on the way in either. The two
//! references below are what an argument takes in place of a name, so a client
//! that means "the blueprint I am looking at" can say which revision it means,
//! and a region is the same shape wherever it is named.
//!
//! Both are pure references to something that exists. Manifest text is not a
//! reference to anything, so an operation that reads text takes it as an
//! argument of its own.
//!
//! [`KeyValueWrite`] lives here for the same reason: run metadata and a
//! gateway's headers are the same pair of strings, and one type for both is one
//! shape a client learns once.

use async_graphql::InputObject;

use super::super::core::blueprints;
use super::super::core::error::ServeError;
use super::super::types::AppState;

/// Which installed blueprint an operation is about.
///
/// The `name` it is installed under, and optionally the `digest` of the
/// revision the caller means. Nothing else: a reference says which blueprint,
/// and an operation that also needs a manifest's text takes that text as an
/// argument of its own.
#[derive(Debug, InputObject)]
pub(crate) struct BlueprintRef {
    /// The installed blueprint's name.
    pub(crate) name: String,
    /// The revision the caller believes is installed, as the lowercase hex
    /// SHA-256 on `Blueprint.digest`.
    ///
    /// Optional, and worth sending: a digest that does not match the installed
    /// manifest fails the request rather than acting on a revision the caller
    /// has not seen.
    pub(crate) digest: Option<String>,
}

impl BlueprintRef {
    /// Point at an installed blueprint by name, with any digest pin checked.
    ///
    /// The lookup happens only for a request that sends a pin. Without one there
    /// is nothing to check, and a name that matches nothing installed stays what
    /// it was: an empty listing or the daemon's own refusal, rather than a walk
    /// of every blueprint directory on the way in.
    pub(crate) async fn installed(self, state: &AppState) -> Result<String, ServeError> {
        let Some(pinned) = self.digest else {
            return Ok(self.name);
        };
        verify_digest(state, &self.name, &pinned).await?;
        Ok(self.name)
    }
}

/// Check a pin against what is installed under that name.
///
/// A mismatch and a name nothing is installed under are both failures: a caller
/// that sent a digest is saying which revision it means, and answering for a
/// different one, or for nothing, is the drift the pin exists to catch.
async fn verify_digest(state: &AppState, name: &str, pinned: &str) -> Result<(), ServeError> {
    let config = state.current_config();
    // Resolved before the walk so a test's blueprint-directory override is
    // visible from the task that reads them, as the blueprint listing does.
    let roots = super::super::blueprints::blueprint_roots(&config);
    let wanted = name.to_string();
    let found = super::super::blocking::blocking(move || {
        super::super::blueprints::discover_in(roots)
            .into_iter()
            .find(|info| info.name == wanted)
            .map(|info| blueprints::digest_of(&info.manifest))
    })
    .await;
    let Some(installed) = found else {
        return Err(ServeError::NotFound(format!(
            "Blueprint '{name}' is not installed, so the digest sent for it cannot be checked"
        )));
    };
    if installed != pinned.trim().to_ascii_lowercase() {
        return Err(ServeError::Conflict(format!(
            "Blueprint '{name}' is installed at digest {installed}, not the {pinned} this \
             request pinned"
        )));
    }
    Ok(())
}

/// One context region, by name.
///
/// An object rather than a string so a region reads the same on the way in as it
/// does on the way out, and so a later field can be added to it without changing
/// the argument's type.
#[derive(Debug, InputObject)]
pub(crate) struct RegionRef {
    /// The region's name, as the blueprint declares it.
    pub(crate) name: String,
}

/// One key and its value, wherever a request writes a pair of strings.
///
/// Run metadata and a gateway's headers are the same shape, so they are the
/// same type: a client that has written one has written both, and neither can
/// drift into spelling the key `name` while the other spells it `key`.
#[derive(Debug, InputObject)]
pub(crate) struct KeyValueWrite {
    /// The key. Given twice in one list, the last value wins.
    pub(crate) key: String,
    /// The value. Always a string.
    pub(crate) value: String,
}

#[cfg(test)]
#[path = "inputs_tests.rs"]
mod tests;
