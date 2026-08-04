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

/// Token budget for [`FINAL_OUTPUT_REGION`]: the byte cap on a submission, in
/// the workspace's generic bytes-over-four estimate. Sized so a submission at
/// the limit still fits the region that mirrors it.
pub const FINAL_OUTPUT_REGION_TOKENS: usize =
    leviath_core::output::MAX_FINAL_OUTPUT_BYTES.div_ceil(4);

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
    stage: &str,
    now: i64,
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

    // The only inspection of the content anywhere, and only because an author
    // asked for it by supplying a schema. A format label never reaches here.
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

    let output = FinalOutput::new(
        content,
        spec.and_then(|s| s.format.clone()),
        stage.to_string(),
        now,
    );
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

/// Mirror the submission into the pinned `final_output` region, replacing
/// whatever was there.
///
/// Best-effort: a world whose layout somehow lacks the region still records the
/// output on the component, which is what every consumer actually reads. The
/// region exists so the answer stays in the agent's own context (a later stage
/// can revise it) and so it appears in `context.json`.
fn mirror_into_region(window: &mut ContextWindow, content: &str) {
    if window.get_region(FINAL_OUTPUT_REGION).is_none() {
        return;
    }
    let tokens = leviath_core::estimate_tokens(content);
    if let Some(region) = window.get_region_mut(FINAL_OUTPUT_REGION) {
        region.clear();
    }
    window.current_tokens = window.calculate_tokens();
    // Through the window method rather than the region directly, so a custom
    // region's `on_write` hook fires - the same reason `context_write` does it.
    let _ = window.add_to_region(FINAL_OUTPUT_REGION, content.to_string(), tokens);
}

#[cfg(test)]
mod tests;
