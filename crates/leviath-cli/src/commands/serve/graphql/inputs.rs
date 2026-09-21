//! Naming a blueprint or a region on the write side.
//!
//! A domain entity is not a bare string on the way in either. These two inputs
//! are what an argument takes in place of a name, so a client that means "the
//! blueprint I am looking at" can say which revision it means, and a region is
//! the same shape wherever it is named.

use async_graphql::InputObject;

use super::super::core::blueprints;
use super::super::core::error::ServeError;
use super::super::types::AppState;

/// Which blueprint an operation is about.
///
/// A pointer at an installed blueprint, as `name` with an optional `digest`
/// pin - or, for an operation that checks a definition rather than pointing at
/// one, `content` with the manifest text. The two are not combined: text to
/// check and a blueprint to act on are different things, and an argument
/// carrying both does not say which the caller meant.
#[derive(Debug, Default, InputObject)]
pub(crate) struct BlueprintInput {
    /// The installed blueprint's name. Required except where `content` carries a
    /// manifest to check, and there it scopes the check to that blueprint's own
    /// directory so its scripts resolve.
    pub(crate) name: Option<String>,
    /// The revision the caller believes is installed, as the lowercase hex
    /// SHA-256 on `Blueprint.digest`.
    ///
    /// Optional, and worth sending: a digest that does not match the installed
    /// manifest fails the request rather than acting on a revision the caller
    /// has not seen. Refused beside `content`, where nothing installed is being
    /// pointed at.
    pub(crate) digest: Option<String>,
    /// The manifest text, for an operation that checks a definition. Refused
    /// where the operation acts on an installed blueprint, which has its own
    /// text already.
    pub(crate) content: Option<String>,
}

/// A blueprint to check, as `validateBlueprint` reads one.
#[derive(Debug)]
pub(crate) struct BlueprintDefinition {
    /// The manifest text.
    pub(crate) content: String,
    /// The installed blueprint whose directory scripts resolve against, when the
    /// request named one.
    pub(crate) name: Option<String>,
}

impl BlueprintInput {
    /// Point at an installed blueprint by name, with any digest pin checked.
    ///
    /// The lookup happens only for a request that sends a pin. Without one there
    /// is nothing to check, and a name that matches nothing installed stays what
    /// it was: an empty listing or the daemon's own refusal, rather than a walk
    /// of every blueprint directory on the way in.
    pub(crate) async fn installed(self, state: &AppState) -> Result<String, ServeError> {
        if self.content.is_some() {
            return Err(ServeError::BadRequest(
                "`content` is a manifest to check, and this argument acts on an installed \
                 blueprint; send `name`"
                    .to_string(),
            ));
        }
        let name = self.name.ok_or_else(|| {
            ServeError::BadRequest(
                "a blueprint argument needs `name`: which installed blueprint to act on"
                    .to_string(),
            )
        })?;
        let Some(pinned) = self.digest else {
            return Ok(name);
        };
        verify_digest(state, &name, &pinned).await?;
        Ok(name)
    }

    /// Read a definition to check, with the blueprint it is checked as.
    pub(crate) fn definition(self) -> Result<BlueprintDefinition, ServeError> {
        if self.digest.is_some() {
            return Err(ServeError::BadRequest(
                "`digest` pins an installed revision, and a check reads the text it is given; \
                 send `content` without it"
                    .to_string(),
            ));
        }
        let content = self.content.ok_or_else(|| {
            ServeError::BadRequest("a check needs `content`: the manifest text to read".to_string())
        })?;
        Ok(BlueprintDefinition {
            content,
            name: self.name,
        })
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
pub(crate) struct RegionInput {
    /// The region's name, as the blueprint declares it.
    pub(crate) name: String,
}

#[cfg(test)]
#[path = "inputs_tests.rs"]
mod tests;
