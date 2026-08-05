//! Handling for the `submit_output` tool: an agent handing back its answer.
//!
//! Applied inline by the tool-dispatch pipeline system rather than on the async
//! tool lane, for the same reason the `context_*` tools are: it writes to the
//! live [`ContextWindow`] and to an ECS component, neither of which the lane can
//! reach. [`handle_output_tool`] is the pure core that system calls.
//!
//! # What this does not do
//!
//! It does not interpret the format. A submission asking for a2ui, XML, CSV, or
//! anything else is recorded byte for byte; the label travels alongside it for
//! consumers to dispatch on, and nothing here reads it.
//!
//! The one exception is opt-in: when the resolved [`OutputSpec`] carries a JSON
//! Schema, the submission is parsed and validated, and a failure is refused back
//! to the model as an `[error] …` result so the next turn can correct it. That
//! refusal path is deliberately the same shape as the dispatch layer's Layer 2
//! argument refusal, down to the `[error]` prefix, which is already in the
//! no-effect list and so keeps a rejected submission from counting as work.
//!
//! [`OutputSpec`]: leviath_core::output::OutputSpec

use leviath_core::output::{FinalOutput, OutputSpec};

use crate::components::ContextWindow;

/// The context region a submitted output is mirrored into.
///
/// Created automatically alongside `conversation` and `tool_results` (see
/// `context_setup`), and pinned, so the answer stays visible to later stages and
/// lands in the run's `context.json` with no extra persistence work.
pub const FINAL_OUTPUT_REGION: &str = "final_output";

/// Token budget for [`FINAL_OUTPUT_REGION`].
///
/// Deliberately far smaller than a maximal answer. The region is a convenience,
/// not the storage: it exists so a later stage can see what was submitted and
/// revise it, while the authoritative copy lives on the component and on disk.
/// Sized at the whole answer, a maximal submission would pin ~65k tokens into
/// every subsequent inference for the rest of the run, which costs more than the
/// convenience is worth. A long answer is mirrored as a preview instead.
pub const FINAL_OUTPUT_REGION_TOKENS: usize = 2_000;

/// Marker appended to a mirrored answer that did not fit the region.
const MIRROR_TRUNCATION_MARKER: &str =
    "\n[...the full answer is on the run's final output, not in context]";

/// Whether a tool name is the final-output tool this module handles.
pub fn is_output_tool(name: &str) -> bool {
    name == leviath_core::blueprint::SUBMIT_OUTPUT_TOOL
}

/// Apply a `submit_output` call.
///
/// Returns the result text the model sees, and the recorded output when the
/// submission was accepted. `None` means nothing was recorded and the model has
/// been told why, so the caller must leave any previously submitted output in
/// place: a rejected correction should not erase a good answer.
///
/// `spec` is the shape resolved for this stage (agent, stage, and caller
/// combined). `None` means no level asked for a particular shape, which is not
/// an error - the stage still wanted an answer, just not a specific form.
pub fn handle_output_tool(
    args: &serde_json::Value,
    spec: Option<&OutputSpec>,
    validators: Option<&crate::components::OutputValidators>,
    stage: &str,
    now: i64,
    workdir: Option<&std::path::Path>,
    window: &mut ContextWindow,
) -> (String, Option<FinalOutput>) {
    let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
        return ("[error] missing 'content' argument".to_string(), None);
    };
    if content.trim().is_empty() {
        return (
            "[error] final output is empty - submit the answer itself, not a placeholder"
                .to_string(),
            None,
        );
    }

    // Is it the format it claims to be? Well-formedness only, and only for the
    // handful of formats this crate can parse - a label it has never seen is
    // carried through unchecked, which is the whole point of an opaque label.
    // Runs before the schema check because "this is not even JSON" is a more
    // useful thing to hear than a list of missing properties.
    if let Some(format) = spec.and_then(|s| s.format.as_deref())
        && let Err(reason) = leviath_tools::validate::format::check(Some(format), content)
    {
        return (
            format!("[error] the final output is not valid {format}: {reason}"),
            None,
        );
    }

    // Does it have the shape the author asked for? Only when they supplied a
    // schema to check against.
    if let Some(schema) = spec.and_then(|s| s.schema.as_ref()) {
        match leviath_tools::validate_output(schema, content) {
            leviath_tools::ArgValidation::Invalid(message) => return (message, None),
            // A schema that will not compile skips the check rather than
            // refusing every submission, matching how tool-argument validation
            // treats the same situation. Refusing here would let one bad schema
            // make a run unable to finish.
            leviath_tools::ArgValidation::SchemaUnusable(e) => {
                tracing::warn!(
                    stage = %stage,
                    error = %e,
                    "output schema did not compile; recording the submission unchecked"
                );
            }
            leviath_tools::ArgValidation::Valid => {}
        }
    }

    // An agent's own validator, for a format nothing here can parse and a shape
    // no JSON Schema can describe. A broken script is reported as broken rather
    // than treated as a rejection: reading it as "the answer is wrong" would
    // burn the retry budget on a script bug and end the run with nothing.
    if let Some(script) = spec.and_then(|s| s.validator.as_deref())
        && let Some(validator) = validators.and_then(|v| v.0.get(script))
    {
        match leviath_scripting::output_validator::validate(validator, content) {
            leviath_scripting::output_validator::Verdict::Invalid(reason) => {
                return (
                    format!("[error] the final output was rejected: {reason}"),
                    None,
                );
            }
            leviath_scripting::output_validator::Verdict::Unusable(reason) => {
                tracing::warn!(
                    stage = %stage,
                    error = %reason,
                    "output validator failed to run; recording the submission unchecked"
                );
            }
            leviath_scripting::output_validator::Verdict::Valid => {}
        }
    }

    // Refused rather than silently dropped: an answer whose artifact list
    // quietly lost an entry sends the caller looking for a file that was named
    // and then forgotten.
    let artifacts = match resolve_artifacts(args, workdir) {
        Ok(paths) => paths,
        Err(message) => return (message, None),
    };

    let output = FinalOutput::new(
        content,
        spec.and_then(|s| s.format.clone()),
        stage.to_string(),
        now,
    )
    .with_artifacts(artifacts);
    mirror_into_region(window, &output.content);

    let mut ack = "Recorded as this run's final output.".to_string();
    if output.truncated {
        ack.push_str(&format!(
            " It exceeded the {} KiB limit and was truncated; submit a shorter answer, or write \
             the long form to a file and summarise it here.",
            leviath_core::output::MAX_FINAL_OUTPUT_BYTES / 1024
        ));
    }
    (ack, Some(output))
}

/// Split the `artifacts` argument into paths that resolve inside `workdir` and
/// paths that do not.
///
/// The same containment rule the files endpoint enforces when serving one, so a
/// path that survives here is a path a consumer can actually fetch. A missing
/// file is fine: an agent may name something it is about to finish writing, and
/// `resolves_within` checks where a path lands rather than whether it exists.
fn resolve_artifacts(
    args: &serde_json::Value,
    workdir: Option<&std::path::Path>,
) -> Result<Vec<String>, String> {
    let listed: Vec<&str> = args
        .get("artifacts")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter(|p| !p.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    if listed.is_empty() {
        return Ok(Vec::new());
    }
    // No workdir means nothing to resolve against, so nothing can be verified.
    // Unreachable for a real run (every one carries its metadata); loud rather
    // than silent if it ever is.
    let Some(workdir) = workdir else {
        return Err(
            "[error] cannot record artifacts: this run has no working directory to resolve \
             them against"
                .to_string(),
        );
    };
    let (kept, rejected): (Vec<&str>, Vec<&str>) = listed
        .into_iter()
        .partition(|p| leviath_core::resolves_within(&workdir.join(p), workdir));
    match rejected.is_empty() {
        true => Ok(kept.into_iter().map(str::to_string).collect()),
        false => Err(format!(
            "[error] these artifact paths do not resolve inside the working directory: {}",
            rejected.join(", ")
        )),
    }
}

/// Mirror the submission into the pinned `final_output` region, replacing
/// whatever was there.
///
/// Best-effort: a world whose layout somehow lacks the region still records the
/// output on the component, which is what every consumer actually reads. The
/// region exists so the answer stays in the agent's own context (a later stage
/// can revise it) and so it appears in `context.json`.
fn mirror_into_region(window: &mut ContextWindow, content: &str) {
    let Some(budget) = window.get_region(FINAL_OUTPUT_REGION).map(|r| r.max_tokens) else {
        return;
    };
    let mirrored = fit_to_region(content, budget);
    let tokens = leviath_core::estimate_tokens(&mirrored);
    if let Some(region) = window.get_region_mut(FINAL_OUTPUT_REGION) {
        region.clear();
    }
    window.current_tokens = window.calculate_tokens();
    // Through the window method rather than the region directly, so a custom
    // region's `on_write` hook fires - the same reason `context_write` does it.
    let _ = window.add_to_region(FINAL_OUTPUT_REGION, mirrored, tokens);
}

/// Trim `content` to fit `budget` tokens, marking it when cut.
///
/// An over-budget entry is *rejected* by `add_entry`, not truncated, so mirroring
/// a long answer without this would leave the region empty - the one outcome
/// worse than a preview.
fn fit_to_region(content: &str, budget: usize) -> String {
    let allowed = budget.saturating_mul(4);
    if content.len() <= allowed {
        return content.to_string();
    }
    let Some(room) = allowed.checked_sub(MIRROR_TRUNCATION_MARKER.len()) else {
        return String::new();
    };
    format!(
        "{}{MIRROR_TRUNCATION_MARKER}",
        leviath_core::truncate_at_boundary(content, room)
    )
}

#[cfg(test)]
mod tests;
