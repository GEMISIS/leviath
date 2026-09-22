//! The `validateBlueprint`, `validateProviderKey` and `validateScript`
//! fields.
//!
//! Text in, verdict out: nothing is written, nothing is dialled, nothing is
//! run. Each one usually precedes a write, which is where it sits in a form,
//! not what it does, so each is a field rather than a mutation.

use async_graphql::Context;

use super::super::super::core::blueprints;
use super::super::super::types::AppState;
use super::super::checks::{KeyVerdict, ScriptVerdict, ValidationReport};
use super::super::error::IntoGraphql;
use super::super::inputs::BlueprintRef;
use super::super::script_ref::ScriptKind;

/// Check a manifest without installing it.
///
/// A failing check is a report, not an error: the request to validate
/// succeeded, and what it found is the answer.
pub(crate) async fn validate_blueprint(
    ctx: &Context<'_>,
    manifest: String,
    as_blueprint: Option<BlueprintRef>,
) -> async_graphql::Result<ValidationReport> {
    let state = ctx.data_unchecked::<AppState>();
    let named = match as_blueprint {
        Some(reference) => Some(reference.installed(state).await.gql()?),
        None => None,
    };
    let dir = match named.as_deref() {
        Some(name) => blueprints::blueprint_dir(name).gql()?,
        None => std::path::PathBuf::new(),
    };
    let report = super::super::super::blueprints::validate_manifest_text(&manifest, &dir);
    Ok(ValidationReport {
        valid: report.valid,
        errors: report.errors.unwrap_or_default(),
        warnings: report.warnings.unwrap_or_default(),
    })
}

/// Whether a provider key looks like one of that provider's.
///
/// Format only: nothing is dialled and nothing is written, which is what
/// makes it safe to run on every keystroke of a form. `checkProvider` is
/// the one that asks the account.
pub(crate) async fn validate_provider_key(
    provider: String,
    key: String,
    base_url: Option<String>,
) -> KeyVerdict {
    let checked = super::super::super::config::checked_key(&provider, &key, base_url.as_deref());
    KeyVerdict {
        valid: checked.valid,
        message: checked.message,
    }
}

/// Whether a script compiles, without writing it.
///
/// The alternative was saving it and waiting for a run to fail, which is
/// not much of an improvement on editing the file over SSH. Ungated:
/// compiling text in memory writes nothing and runs nothing, because every
/// compiler here stops at the syntax tree.
pub(crate) async fn validate_script(
    kind: ScriptKind,
    content: String,
    required_hooks: Option<Vec<String>>,
) -> async_graphql::Result<ScriptVerdict> {
    let hooks = required_hooks.unwrap_or_default();
    let named: Vec<&str> = hooks.iter().map(String::as_str).collect();
    let checked = super::super::super::scripts::compiled(kind.wire(), &content, &named).gql()?;
    Ok(ScriptVerdict {
        valid: checked.valid,
        error: checked.error,
    })
}
