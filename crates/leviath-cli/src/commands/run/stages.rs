//! Stage runner functions: interactive, autonomous, interactive_points.

use leviath_core::lifecycle::CompactionConfig;
use leviath_runtime::{AgentEngine, ToolExecutorDyn};

use crate::runstate::{self, RunMeta};

use super::helpers::record_stage_log;
use super::helpers::record_stage_output;
use super::inference::stream_inference;
use super::io::RunIO;

/// Normalize a string for followup matching: collapse whitespace and
/// normalize Unicode dashes (em dash `\u{2014}`, en dash `\u{2013}`,
/// minus sign `\u{2212}`, horizontal bar `\u{2015}`) to ASCII hyphen-minus.
fn normalize_for_followup(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2014}' | '\u{2013}' | '\u{2212}' | '\u{2015}' => '-',
            _ => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Look up a directive in the directives map, trying exact match first,
/// then normalized comparison (whitespace + Unicode dash normalization).
fn lookup_directive<'a>(
    directives: &'a std::collections::HashMap<String, String>,
    user_text: &str,
) -> Option<&'a str> {
    // Exact match first
    if let Some(directive) = directives.get(user_text) {
        return Some(directive.as_str());
    }
    // Normalized match
    let normalized = normalize_for_followup(user_text);
    for (key, directive) in directives {
        if normalize_for_followup(key) == normalized {
            tracing::debug!(
                user_text = %user_text,
                key = %key,
                "Directive matched via normalization (original text didn't match exactly)"
            );
            return Some(directive.as_str());
        }
    }
    None
}

/// Whether `user_text` matches one of the given option labels (exact match
/// first, then the same normalization used for directive lookup). Shared by
/// the abort- and edit-option checks.
fn option_matches(candidates: &[String], user_text: &str) -> bool {
    if candidates.iter().any(|o| o == user_text) {
        return true;
    }
    let normalized = normalize_for_followup(user_text);
    candidates
        .iter()
        .any(|o| normalize_for_followup(o) == normalized)
}

/// Whether `user_text` matches one of the point's abort options.
fn is_abort_option(abort_options: &[String], user_text: &str) -> bool {
    option_matches(abort_options, user_text)
}

/// Whether `user_text` matches one of the point's edit options.
fn is_edit_option(edit_options: &[String], user_text: &str) -> bool {
    option_matches(edit_options, user_text)
}

/// Outcome of running an interactive-points stage. `Aborted` signals the
/// executor to cancel the run immediately — no final inference and no stage
/// transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointsOutcome {
    Completed,
    Aborted,
}

/// Run an interactive stage.
///
/// `run_context`: if `Some((run_id, meta))`, interaction is handled via the
/// file-based IPC channel (background worker). If `None`, stdin is used
/// (foreground) via `io.get_user_input()`.
#[allow(clippy::too_many_arguments)]
pub async fn run_interactive_stage(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    run_context: Option<(&str, &mut RunMeta)>,
    stage_name: &str,
    io: &mut dyn RunIO,
    executor: &mut ToolExecutorDyn<'_>,
) -> anyhow::Result<()> {
    use crate::interaction::{
        make_interaction_id, request_interaction_async, response_as_text, InteractionRequest,
    };
    use leviath_runtime::ContextWindow;

    let has_tools = !tools.is_empty();
    let mut turn = 0;

    // We need to hold the run_id separately since we consume run_context's meta
    // across iterations. Decouple them to avoid borrow issues.
    let (run_id_owned, meta_opt): (Option<String>, Option<&mut RunMeta>) = match run_context {
        Some((rid, m)) => (Some(rid.to_string()), Some(m)),
        None => (None, None),
    };

    // We need meta across loop iterations — box it optionally.
    let mut meta_holder = meta_opt;

    loop {
        if turn >= max_iterations {
            io.on_output("\n[Max turns reached]\n").await;
            break;
        }

        if has_tools {
            let per_turn_iters = 10_usize.min(max_iterations.saturating_sub(turn));
            let response = engine
                .run_inference_loop_filtered_dyn(
                    entity,
                    provider_name,
                    model_name,
                    tools.to_vec(),
                    per_turn_iters,
                    None,
                    None,
                    executor,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Inference error: {}", e))?;

            io.on_output(&format!("\nAssistant: {}", response.content))
                .await;
            io.on_tokens(
                response.tokens_used.prompt_tokens,
                response.tokens_used.completion_tokens,
                response.tokens_used.cached_tokens,
            )
            .await;

            // Route to per-stage files so the dashboard can display them.
            let token_line = format!(
                "[Tokens: {} in, {} out]",
                response.tokens_used.prompt_tokens, response.tokens_used.completion_tokens
            );
            if let (Some(run_id), Some(ref m)) = (&run_id_owned, &meta_holder) {
                record_stage_output(run_id, m.stage_index, &response.content);
                record_stage_log(run_id, m.stage_index, &token_line);
            }

            // Update meta token counts so the dashboard shows them before the
            // next interaction point (before they go to WaitingInput).
            if let Some(ref mut m) = meta_holder {
                m.prompt_tokens += response.tokens_used.prompt_tokens;
                m.completion_tokens += response.tokens_used.completion_tokens;
                m.cached_tokens += response.tokens_used.cached_tokens;
                m.touch();
                let _ = runstate::write_meta(m);
            }

            let _tokens_hint = response.content.len() / 4 + 1;
            let _window_result =
                engine
                    .world_mut()
                    .get_mut::<ContextWindow>(entity)
                    .map(|mut w| {
                        w.add_typed_entry(
                            "conversation",
                            leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
                            response.content.clone(),
                            _tokens_hint,
                        )
                    });
        } else {
            let response =
                match stream_inference(engine, entity, provider_name, model_name, None, tools, io)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::debug!("Streaming unavailable, falling back: {}", e);
                        let r = engine
                            .run_inference_filtered(
                                entity,
                                provider_name,
                                model_name,
                                Vec::new(),
                                None,
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!("Inference error: {}", e))?;
                        io.on_output(&format!("\nAssistant: {}", r.content)).await;
                        r
                    }
                };

            io.on_tokens(
                response.tokens_used.prompt_tokens,
                response.tokens_used.completion_tokens,
                response.tokens_used.cached_tokens,
            )
            .await;

            if let Some(ref mut m) = meta_holder {
                m.prompt_tokens += response.tokens_used.prompt_tokens;
                m.completion_tokens += response.tokens_used.completion_tokens;
                m.cached_tokens += response.tokens_used.cached_tokens;
                m.touch();
                let _ = runstate::write_meta(m);
            }

            let _tokens_hint2 = response.content.len() / 4 + 1;
            let _window_result2 =
                engine
                    .world_mut()
                    .get_mut::<ContextWindow>(entity)
                    .map(|mut w| {
                        w.add_typed_entry(
                            "conversation",
                            leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
                            response.content.clone(),
                            _tokens_hint2,
                        )
                    });
        }

        // Build and dispatch the input request
        let req = InteractionRequest::free_text(
            make_interaction_id(0, turn),
            "Your response (leave empty or /quit to end):",
            stage_name,
            false, // not required — empty ends the loop
        );

        let input = if let (Some(run_id), Some(ref mut meta)) = (&run_id_owned, &mut meta_holder) {
            let resp = request_interaction_async(run_id, meta, req, None).await?;
            response_as_text(&resp)
        } else {
            // Foreground path: use RunIO for user input
            io.get_user_input("Your response (leave empty or /quit to end):")
                .await
                .unwrap_or_default()
        };

        if input.is_empty() || input == "/quit" || input == "/exit" {
            io.on_output("\n[Session ended]\n").await;
            break;
        }

        let _user_tokens = input.len() / 4 + 1;
        let _user_window_result =
            engine
                .world_mut()
                .get_mut::<ContextWindow>(entity)
                .map(|mut w| {
                    w.add_typed_entry(
                        "conversation",
                        leviath_core::EntryKind::UserMessage,
                        input.clone(),
                        _user_tokens,
                    )
                });

        turn += 1;
    }

    Ok(())
}

/// Run an autonomous stage with the real tool executor.
#[allow(clippy::too_many_arguments)]
pub async fn run_autonomous_stage(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    compaction_config: Option<&CompactionConfig>,
    io: &mut dyn RunIO,
    executor: &mut ToolExecutorDyn<'_>,
) -> anyhow::Result<()> {
    let response = engine
        .run_inference_loop_filtered_dyn(
            entity,
            provider_name,
            model_name,
            tools.to_vec(),
            max_iterations,
            None,
            compaction_config,
            executor,
        )
        .await;

    match response {
        Ok(resp) => {
            io.on_output(&resp.content).await;
            io.on_tokens(
                resp.tokens_used.prompt_tokens,
                resp.tokens_used.completion_tokens,
                resp.tokens_used.cached_tokens,
            )
            .await;
        }
        Err(e) => {
            io.on_error(&format!("Inference error: {}", e)).await;
        }
    }
    Ok(())
}

/// Placeholder foreground asker bound only in background (IPC) mode, where the
/// stdin-dispatch branch of [`run_interactive_points_stage_with`] is never
/// taken. Returns an empty text response. Directly unit-tested so its body is
/// covered even though the interaction loop never invokes it in background mode.
fn ipc_mode_unused_asker(
    req: &crate::interaction::InteractionRequest,
) -> crate::interaction::InteractionResponse {
    crate::interaction::InteractionResponse::text(&req.id, "")
}

/// Run an InteractivePoints stage: autonomous iterations with pauses at each interaction point.
///
/// `run_context`: if `Some((run_id, meta))`, interaction is handled via the
/// file-based IPC channel (background worker). If `None`, stdin is used
/// (foreground) and `asker` must be `Some(..)`.
///
/// `asker` is the foreground stdin asker (from
/// [`StageCallbacks::interaction_point_asker`](super::executor::StageCallbacks::interaction_point_asker)).
/// It is required only on the foreground path (`run_context == None` with
/// interaction points); background runs pass `None` and resolve via IPC.
#[allow(clippy::too_many_arguments)]
pub async fn run_interactive_points_stage(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    compaction_config: Option<&CompactionConfig>,
    points: &[leviath_core::blueprint::InteractionPoint],
    run_context: Option<(&str, &mut RunMeta)>,
    io: &mut dyn RunIO,
    executor: &mut ToolExecutorDyn<'_>,
    asker: Option<super::executor::InteractionAsker>,
) -> anyhow::Result<PointsOutcome> {
    // Resolve the foreground asker into the `&dyn Fn` the core takes. In
    // foreground mode (no run_context) it is required; background mode resolves
    // interaction points via IPC and never consults it, so a placeholder is
    // bound there.
    let ask: &(dyn Fn(&crate::interaction::InteractionRequest) -> crate::interaction::InteractionResponse
          + Sync) = match asker {
        Some(ref f) => f,
        // Background (IPC) mode and the empty-points autonomous fallback never
        // consult the asker, so bind the placeholder there rather than bailing.
        None if run_context.is_some() || points.is_empty() => &ipc_mode_unused_asker,
        None => anyhow::bail!("interactive points in foreground mode require an interaction asker"),
    };
    run_interactive_points_stage_with(
        engine,
        entity,
        provider_name,
        model_name,
        max_iterations,
        tools,
        compaction_config,
        points,
        run_context,
        io,
        executor,
        ask,
    )
    .await
}

/// Core of [`run_interactive_points_stage`], with the foreground (stdin)
/// interaction dispatch injected as `ask_foreground` so tests can drive the
/// `run_context = None` path with a mock closure instead of blocking on real
/// process stdin. Production callers go through the public wrapper above, which
/// supplies the injected asker (or a background placeholder).
#[allow(clippy::too_many_arguments)]
async fn run_interactive_points_stage_with(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    compaction_config: Option<&CompactionConfig>,
    points: &[leviath_core::blueprint::InteractionPoint],
    run_context: Option<(&str, &mut RunMeta)>,
    io: &mut dyn RunIO,
    executor: &mut ToolExecutorDyn<'_>,
    ask_foreground: &(dyn Fn(&crate::interaction::InteractionRequest) -> crate::interaction::InteractionResponse
          + Sync),
) -> anyhow::Result<PointsOutcome> {
    use crate::interaction::{
        make_interaction_id, request_interaction_async, response_as_choice, response_as_text,
        InteractionRequest,
    };
    use leviath_runtime::ContextWindow;

    if points.is_empty() {
        run_autonomous_stage(
            engine,
            entity,
            provider_name,
            model_name,
            max_iterations,
            tools,
            compaction_config,
            io,
            executor,
        )
        .await
        .expect(
            "infallible: run_autonomous_stage catches inference errors and always returns Ok(())",
        );
        return Ok(PointsOutcome::Completed);
    }

    let (run_id_owned, mut meta_holder): (Option<String>, Option<&mut RunMeta>) = match run_context
    {
        Some((rid, m)) => (Some(rid.to_string()), Some(m)),
        None => (None, None),
    };

    let segments = points.len() + 1;
    let iterations_per_segment = max_iterations / segments;
    let mut remaining_iterations = max_iterations;

    // The most recent non-empty inference output for the current stage (e.g.
    // the plan). Used to seed an "edit" option's editable field with the text
    // the user is choosing to modify.
    let mut last_output: Option<String> = None;

    // Cap how many times a single interaction point can loop back on itself
    // via a followup (e.g. repeatedly picking "Revise"). Bounded independently
    // of the iteration budget so a chatty user can't spin forever.
    const MAX_REVISION_ROUNDS: usize = 4;

    for (pt_idx, point) in points.iter().enumerate() {
        let mut revision_round = 0usize;

        'point: loop {
            let iters = iterations_per_segment.min(remaining_iterations);
            if iters > 0 {
                let response = engine
                    .run_inference_loop_filtered_dyn(
                        entity,
                        provider_name,
                        model_name,
                        tools.to_vec(),
                        iters,
                        None,
                        compaction_config,
                        executor,
                    )
                    .await;

                if let Ok(resp) = response {
                    if !resp.content.is_empty() {
                        io.on_output(&resp.content).await;
                        // Route agent response to the per-stage output file so the dashboard can display it
                        if let (Some(run_id), Some(ref m)) = (&run_id_owned, &meta_holder) {
                            record_stage_output(run_id, m.stage_index, &resp.content);
                        }
                        last_output = Some(resp.content.clone());
                    }
                    // Update token counts in meta so the dashboard shows them before WaitingInput
                    if let Some(ref mut m) = meta_holder {
                        let token_line = format!(
                            "[Tokens: {} in, {} out]",
                            resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
                        );
                        record_stage_log(&m.run_id, m.stage_index, &token_line);
                        m.prompt_tokens += resp.tokens_used.prompt_tokens;
                        m.completion_tokens += resp.tokens_used.completion_tokens;
                        m.cached_tokens += resp.tokens_used.cached_tokens;
                        m.touch();
                        let _ = runstate::write_meta(m);
                    }
                }
                remaining_iterations = remaining_iterations.saturating_sub(iters);
            }

            // Build the interaction request with the right style / options
            let req_id = make_interaction_id(pt_idx, revision_round * 2);
            let bp_style = &point.style;
            let ipc_req = match bp_style {
                leviath_core::blueprint::InteractionStyle::MultipleChoice => {
                    InteractionRequest::multiple_choice(
                        req_id,
                        &point.prompt,
                        point.options.clone(),
                        &point.name,
                    )
                }
                leviath_core::blueprint::InteractionStyle::Confirm => {
                    InteractionRequest::confirm(req_id, &point.prompt, &point.name)
                }
                leviath_core::blueprint::InteractionStyle::FreeText => {
                    InteractionRequest::free_text(
                        req_id,
                        &point.prompt,
                        &point.name,
                        point.required,
                    )
                }
            };

            // Dispatch via file IPC or stdin
            let user_text = if let (Some(run_id), Some(ref mut meta)) =
                (&run_id_owned, &mut meta_holder)
            {
                let resp = request_interaction_async(run_id, meta, ipc_req.clone(), None).await?;
                match bp_style {
                    leviath_core::blueprint::InteractionStyle::MultipleChoice
                    | leviath_core::blueprint::InteractionStyle::Confirm => {
                        // Resolve choice index → option string
                        response_as_choice(&resp, &ipc_req.options)
                            .cloned()
                            .unwrap_or_else(|| response_as_text(&resp))
                    }
                    leviath_core::blueprint::InteractionStyle::FreeText => response_as_text(&resp),
                }
            } else {
                // Foreground (stdin) path — dispatched through the injected
                // `ask_foreground` closure (the binary's real stdin asker in
                // production, a mock in tests) rather than reading stdin
                // directly, so this whole branch is testable without
                // blocking on real stdin.
                let resp = ask_foreground(&ipc_req);
                match bp_style {
                    leviath_core::blueprint::InteractionStyle::MultipleChoice
                    | leviath_core::blueprint::InteractionStyle::Confirm => {
                        response_as_choice(&resp, &ipc_req.options)
                            .cloned()
                            .unwrap_or_else(|| response_as_text(&resp))
                    }
                    leviath_core::blueprint::InteractionStyle::FreeText => response_as_text(&resp),
                }
            };

            // Deterministic abort: if the selection is an abort option, signal
            // the executor to cancel the run immediately. No label injection,
            // no final inference, no transition — the executor's `on_cancel`
            // owns marking the run Cancelled (consistent with `on_stage_error`).
            if is_abort_option(&point.abort_options, &user_text) {
                tracing::info!(
                    point = %point.name,
                    selection = %user_text,
                    "Interaction point aborted by user"
                );
                return Ok(PointsOutcome::Aborted);
            }

            if !user_text.is_empty() {
                if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                    let tokens = user_text.len() / 4 + 1;
                    let content = format!("User [{}]: {}", point.name, user_text);
                    let _ = window.add_to_region("conversation", content, tokens);
                }
            }

            // Deterministic direct-edit: if the selection is an edit option,
            // the engine itself opens the stage's most recent output in an
            // editable field (seeded via `body`) and injects the user's edited
            // text back into context, then loops back to re-present the point.
            // Unlike a directive, this does NOT depend on the model choosing to
            // call an edit tool — the editor always appears.
            if is_edit_option(&point.edit_options, &user_text) {
                if revision_round + 1 >= MAX_REVISION_ROUNDS || remaining_iterations == 0 {
                    break 'point;
                }
                let edit_req_id = make_interaction_id(pt_idx, revision_round * 2 + 1);
                let seed = last_output.clone().unwrap_or_default();
                let edit_req = InteractionRequest::edit_text(
                    edit_req_id,
                    "Edit the text below, then submit your changes:",
                    &point.name,
                    seed,
                );
                let edited =
                    if let (Some(run_id), Some(ref mut meta)) = (&run_id_owned, &mut meta_holder) {
                        let resp = request_interaction_async(run_id, meta, edit_req, None).await?;
                        response_as_text(&resp)
                    } else {
                        response_as_text(&ask_foreground(&edit_req))
                    };
                if !edited.is_empty() {
                    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                        let content = format!(
                            "User [{}] edited the output directly. Adopt this exact text as the \
                             authoritative version and re-present it:\n{}",
                            point.name, edited
                        );
                        let tokens = content.len() / 4 + 1;
                        let _ = window.add_to_region("conversation", content, tokens);
                    }
                    // Reflect the edit as the current output too, so the next
                    // seed (if edited again) and the dashboard stay in sync.
                    last_output = Some(edited);
                }
                revision_round += 1;
                continue 'point;
            }

            // If the chosen option has a configured directive, inject it into
            // the agent's context and loop back to re-run inference IN-STAGE.
            // The agent reads the directive and drives the next step itself
            // (e.g. calling `ask_user_text` or `edit_document`), then the same
            // point is re-presented — deterministic routing, no fall-through to
            // a stage transition. A plain option (no directive) breaks and
            // completes the stage normally.
            let Some(directive) = lookup_directive(&point.directives, &user_text) else {
                // Collect the directive keys eagerly into a local so the
                // expression is evaluated regardless of whether a debug-level
                // subscriber is installed (tracing defers field evaluation when
                // the level is disabled, which otherwise leaves it uncovered).
                let directive_keys: Vec<_> = point.directives.keys().collect();
                tracing::debug!(
                    user_text = %user_text,
                    directive_keys = ?directive_keys,
                    "No directive match found for user selection — completing stage"
                );
                break 'point;
            };
            if revision_round + 1 >= MAX_REVISION_ROUNDS || remaining_iterations == 0 {
                break 'point;
            }

            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let content = format!("User [{}] directive: {}", point.name, directive);
                let tokens = content.len() / 4 + 1;
                let _ = window.add_to_region("conversation", content, tokens);
            }

            revision_round += 1;
        }
    }

    if remaining_iterations > 0 {
        let response = engine
            .run_inference_loop_filtered_dyn(
                entity,
                provider_name,
                model_name,
                tools.to_vec(),
                remaining_iterations,
                None,
                compaction_config,
                executor,
            )
            .await;

        if let Ok(resp) = response {
            if !resp.content.is_empty() {
                io.on_output(&resp.content).await;
                if let (Some(run_id), Some(ref m)) = (&run_id_owned, &meta_holder) {
                    record_stage_output(run_id, m.stage_index, &resp.content);
                }
            }
            io.on_tokens(
                resp.tokens_used.prompt_tokens,
                resp.tokens_used.completion_tokens,
                resp.tokens_used.cached_tokens,
            )
            .await;
            if let (Some(_), Some(ref m)) = (&run_id_owned, &meta_holder) {
                let token_line = format!(
                    "[Tokens used: {} input, {} output]",
                    resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
                );
                record_stage_log(&m.run_id, m.stage_index, &token_line);
            }
        }
    }

    Ok(PointsOutcome::Completed)
}

#[cfg(test)]
mod tests {
    use super::super::helpers::initialize_context_window;
    use super::super::io::mock::MockIO;
    use super::*;
    use async_trait::async_trait;
    use leviath_core::blueprint::ModelConfig;
    use leviath_core::layout::RegionDefinition;
    use leviath_core::{Blueprint, ContextLayout, EvictionStrategy, RegionKind, Stage};
    use leviath_providers::{
        FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo, Provider,
        ProviderError, TokenUsage,
    };
    use leviath_runtime::{AgentPool, ProviderRegistry};
    use std::sync::Arc;

    use crate::test_support::with_tracing;

    /// Shared `assert!`-with-dynamic-message helper for the many tests that
    /// assert a condition on `io.outputs`/`io.errors` while formatting the
    /// full `Vec<String>` into the panic message for diagnostics if the
    /// assertion ever fails. The panic-message formatting is only evaluated
    /// on failure, which otherwise leaves it permanently uncovered by
    /// `cargo llvm-cov`. Extracted once here (rather than per call site) and
    /// exercised below via `#[should_panic]`.
    fn assert_contains_debug(cond: bool, prefix: &str, value: &[String]) {
        assert!(cond, "{}: {:?}", prefix, value);
    }

    #[test]
    #[should_panic(expected = "Expected max turns message in outputs: [\"nope\"]")]
    fn assert_contains_debug_panics_when_false() {
        assert_contains_debug(
            false,
            "Expected max turns message in outputs",
            &["nope".to_string()],
        );
    }

    /// Same purpose as [`assert_contains_debug`] above, but for the tests
    /// that format a single joined `String` (rather than a `Vec<String>`)
    /// into the panic message.
    fn assert_contains_display(cond: bool, prefix: &str, value: &str) {
        assert!(cond, "{}: {}", prefix, value);
    }

    #[test]
    #[should_panic(expected = "expected stage output to be recorded, got: nope")]
    fn assert_contains_display_panics_when_false() {
        assert_contains_display(false, "expected stage output to be recorded, got", "nope");
    }

    /// A mock provider that returns canned responses for testing.
    struct MockProvider {
        response_content: String,
    }

    impl MockProvider {
        fn new(content: &str) -> Self {
            Self {
                response_content: content.to_string(),
            }
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn infer(
            &self,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, ProviderError> {
            Ok(InferenceResponse {
                content: self.response_content.clone(),
                tool_calls: vec![],
                tokens_used: TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 2,
                    cache_write_tokens: 0,
                },
                finish_reason: FinishReason::Complete,
            })
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "mock"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }
    }

    fn make_blueprint(stages: Vec<Stage>) -> Blueprint {
        let layout = ContextLayout::new(
            vec![
                RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
                RegionDefinition::new(
                    "conversation".to_string(),
                    RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: EvictionStrategy::PerItem,
                    },
                    10000,
                ),
            ],
            12000,
        );
        Blueprint::new("test".to_string(), "test agent".to_string(), stages, layout)
    }

    fn make_stage(name: &str) -> Stage {
        Stage::new(
            name.to_string(),
            ModelConfig::new("mock".to_string(), "test-model".to_string()),
        )
    }

    fn make_engine_and_entity(
        blueprint: &Blueprint,
        provider_content: &str,
    ) -> (
        leviath_runtime::AgentEngine,
        AgentPool,
        bevy_ecs::prelude::Entity,
    ) {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::new(provider_content)),
        );
        let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, blueprint, "test task");
        (engine, pool, entity)
    }

    /// Like [`make_engine_and_entity`], but registers `provider` under
    /// `provider_name` instead of the fixed `MockProvider`/`"mock"` pair --
    /// used for providers with different `infer`/`infer_stream` behavior
    /// (e.g. [`StreamFailingProvider`]).
    fn make_engine_and_entity_with_provider(
        blueprint: &Blueprint,
        provider_name: &str,
        provider: Arc<dyn Provider>,
    ) -> (
        leviath_runtime::AgentEngine,
        AgentPool,
        bevy_ecs::prelude::Entity,
    ) {
        let mut registry = ProviderRegistry::new();
        registry.register(provider_name.to_string(), provider);
        let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, blueprint, "test task");
        (engine, pool, entity)
    }

    /// `infer_stream` always fails; `infer` (used by `run_interactive_stage`'s
    /// non-streaming fallback) succeeds -- exercises the "streaming
    /// unavailable, falling back" branch in the tool-less path.
    struct StreamFailingProvider {
        response_content: String,
    }

    #[async_trait]
    impl Provider for StreamFailingProvider {
        async fn infer(
            &self,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, ProviderError> {
            Ok(InferenceResponse {
                content: self.response_content.clone(),
                tool_calls: vec![],
                tokens_used: TokenUsage {
                    prompt_tokens: 3,
                    completion_tokens: 2,
                    total_tokens: 5,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: FinishReason::Complete,
            })
        }

        async fn infer_stream(
            &self,
            _request: InferenceRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<leviath_providers::StreamChunk, ProviderError>,
                        > + Send,
                >,
            >,
            ProviderError,
        > {
            Err(ProviderError::Other("stream unavailable".to_string()))
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "stream-failing-mock"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }
    }

    fn noop_exec(
        _calls: Vec<leviath_providers::ToolCall>,
    ) -> leviath_runtime::ToolResultsFuture<'static> {
        Box::pin(std::future::ready(vec![]))
    }

    // ─── Tests ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn interactive_stage_max_turns_outputs_message() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Hello from assistant");
        let mut io = MockIO::new();

        // max_iterations=0 means it immediately hits the limit
        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            0, // max_iterations
            &[],
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert_contains_debug(
            io.outputs.iter().any(|o| o.contains("[Max turns reached]")),
            "Expected max turns message in outputs",
            &io.outputs,
        );
    }

    #[tokio::test]
    async fn interactive_stage_quit_ends_session() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Hello from assistant");
        let mut io = MockIO::new().with_inputs(vec!["/quit".to_string()]);

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[], // no tools → uses stream_inference path
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert_contains_debug(
            io.outputs.iter().any(|o| o.contains("[Session ended]")),
            "Expected session ended message in outputs",
            &io.outputs,
        );
    }

    #[tokio::test]
    async fn interactive_stage_empty_input_ends_session() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Hi");
        // MockIO returns None when inputs are exhausted → unwrap_or_default → empty string → quit
        let mut io = MockIO::new();

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[],
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert_contains_debug(
            io.outputs.iter().any(|o| o.contains("[Session ended]")),
            "Expected session ended in outputs",
            &io.outputs,
        );
    }

    #[tokio::test]
    async fn interactive_stage_stream_unavailable_falls_back_to_non_streaming() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity_with_provider(
            &bp,
            "stream-fail",
            Arc::new(StreamFailingProvider {
                response_content: "fallback content".to_string(),
            }),
        );
        let mut io = MockIO::new().with_inputs(vec!["/quit".to_string()]);
        let mut exec = noop_exec;

        // Wrapped in `with_tracing` so the "Streaming unavailable, falling
        // back" debug!'s field-argument line is exercised.
        with_tracing(|| {
            run_interactive_stage(
                &mut engine,
                entity,
                "stream-fail",
                "test-model",
                10,
                &[], // no tools → uses stream_inference path, which fails here
                None,
                "main",
                &mut io,
                &mut exec,
            )
        })
        .await
        .unwrap();

        assert_contains_debug(
            io.outputs
                .iter()
                .any(|o| o.contains("Assistant: fallback content")),
            "Expected fallback response in outputs",
            &io.outputs,
        );
    }

    #[tokio::test]
    async fn interactive_stage_with_run_context_and_tools_records_meta_and_output() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_stage_with_run_context_and_tools_records_meta_and_output",
        );
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent reply with tools");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-is-tools-ctx-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![crate::interaction::InteractionResponse::text("", "")],
        );

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[leviath_providers::Tool {
                name: "noop".to_string(),
                description: "does nothing".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }],
            Some((&run_id, &mut meta)),
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        responder.abort();

        // Tokens are now cumulative across all inference calls in the loop.
        // The mock returns 10 prompt / 5 completion per call; the exact total
        // depends on how many iterations the loop runs.
        assert!(
            meta.prompt_tokens >= 10,
            "expected cumulative prompt tokens >= 10, got {}",
            meta.prompt_tokens
        );
        assert!(
            meta.completion_tokens >= 5,
            "expected cumulative completion tokens >= 5, got {}",
            meta.completion_tokens
        );
        let output = crate::runstate::tail_stage_output(&run_id, meta.stage_index, 65536);
        assert_contains_display(
            output.contains("Agent reply with tools"),
            "expected stage output to be recorded, got",
            &output,
        );

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_stage_with_run_context_no_tools_records_meta() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_stage_with_run_context_no_tools_records_meta",
        );
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Streamed agent reply");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-is-notools-ctx-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![crate::interaction::InteractionResponse::text("", "")],
        );

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[], // no tools → tool-less streaming path, background run_context
            Some((&run_id, &mut meta)),
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        responder.abort();

        assert_eq!(meta.prompt_tokens, 10);
        assert_eq!(meta.completion_tokens, 5);

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[test]
    fn mock_provider_trivial_trait_methods() {
        let provider = MockProvider::new("content");
        assert_eq!(provider.count_tokens("abcd", "m"), 1);
        assert_eq!(provider.max_context_tokens("m"), 100_000);
        assert_eq!(provider.name(), "mock");
        assert!(tokio_test_block_on(provider.list_models())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn stream_failing_provider_trivial_trait_methods() {
        let provider = StreamFailingProvider {
            response_content: "content".to_string(),
        };
        assert_eq!(provider.count_tokens("abcd", "m"), 1);
        assert_eq!(provider.max_context_tokens("m"), 100_000);
        assert_eq!(provider.name(), "stream-failing-mock");
        assert!(tokio_test_block_on(provider.list_models())
            .unwrap()
            .is_empty());
    }

    fn tokio_test_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(fut)
    }

    #[tokio::test]
    async fn noop_exec_returns_empty_vec() {
        let out = noop_exec(vec![]).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn interactive_stage_streams_assistant_output() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Test response content");
        let mut io = MockIO::new().with_inputs(vec!["/quit".to_string()]);

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[],
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        // The streaming path outputs the response content via io
        let all_output: String = io.outputs.join("");
        assert_contains_debug(
            all_output.contains("Test response content"),
            "Expected assistant output in",
            &io.outputs,
        );
    }

    #[tokio::test]
    async fn interactive_stage_reports_tokens() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new().with_inputs(vec!["/quit".to_string()]);

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[],
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        // Token records should have been reported
        assert!(!io.token_records.is_empty());
    }

    #[tokio::test]
    async fn autonomous_stage_outputs_response() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Autonomous result");
        let mut io = MockIO::new();

        run_autonomous_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            1,
            &[],
            None,
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        let all_output: String = io.outputs.join("");
        assert_contains_debug(
            all_output.contains("Autonomous result"),
            "Expected response content in outputs",
            &io.outputs,
        );
    }

    #[tokio::test]
    async fn autonomous_stage_reports_tokens() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new();

        run_autonomous_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            1,
            &[],
            None,
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert_eq!(io.token_records.len(), 1);
        let (prompt, completion, cached) = io.token_records[0];
        assert_eq!(prompt, 10);
        assert_eq!(completion, 5);
        assert_eq!(cached, 2);
    }

    #[tokio::test]
    async fn autonomous_stage_error_uses_io() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "unused");
        let mut io = MockIO::new();

        run_autonomous_stage(
            &mut engine,
            entity,
            "nonexistent", // provider doesn't exist
            "test-model",
            1,
            &[],
            None,
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert_contains_debug(
            io.errors.iter().any(|e| e.contains("Inference error")),
            "Expected inference error in errors",
            &io.errors,
        );
    }

    #[tokio::test]
    async fn interactive_points_empty_delegates_to_autonomous() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Points result");
        let mut io = MockIO::new();

        // Empty points → delegates to run_autonomous_stage
        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            1,
            &[],
            None,
            &[], // empty points
            None,
            &mut io,
            &mut noop_exec,
            // Foreground (run_context None), but empty points delegate to the
            // autonomous path, so the asker is never consulted.
            Some(wrapper_mock_asker),
        )
        .await
        .unwrap();

        let all_output: String = io.outputs.join("");
        assert_contains_debug(
            all_output.contains("Points result"),
            "Expected autonomous output",
            &io.outputs,
        );
    }

    #[tokio::test]
    async fn interactive_stage_with_tools_works() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Tool response");
        let mut io = MockIO::new().with_inputs(vec!["/quit".to_string()]);

        // Provide a tool so the tool-path is taken
        let tools = vec![leviath_providers::Tool {
            name: "test_tool".to_string(),
            description: "a test tool".to_string(),
            parameters: serde_json::json!({}),
        }];

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &tools,
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        // Should have gotten output from the tool-path
        let all_output: String = io.outputs.join("");
        assert_contains_debug(
            all_output.contains("Tool response"),
            "Expected tool response in outputs",
            &io.outputs,
        );
    }

    #[tokio::test]
    async fn interactive_stage_exit_ends_session() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new().with_inputs(vec!["/exit".to_string()]);

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[],
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert_contains_debug(
            io.outputs.iter().any(|o| o.contains("[Session ended]")),
            "Expected session ended message",
            &io.outputs,
        );
    }

    #[tokio::test]
    async fn autonomous_stage_no_errors() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "OK");
        let mut io = MockIO::new();

        let result = run_autonomous_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            5,
            &[],
            None,
            &mut io,
            &mut noop_exec,
        )
        .await;

        assert!(result.is_ok());
        assert!(io.errors.is_empty());
    }

    #[tokio::test]
    async fn interactive_stage_multiple_turns() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Reply");
        let mut io =
            MockIO::new().with_inputs(vec!["first question".to_string(), "/quit".to_string()]);

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[],
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        // Should have multiple outputs (at least 2: response + session ended)
        assert_contains_debug(
            io.outputs.len() >= 2,
            "Expected at least 2 outputs",
            &io.outputs,
        );
    }

    // ─── run_interactive_points_stage: with actual interaction points ──────

    fn make_free_text_point(name: &str, prompt: &str) -> leviath_core::blueprint::InteractionPoint {
        leviath_core::blueprint::InteractionPoint {
            name: name.to_string(),
            prompt: prompt.to_string(),
            required: false,
            style: leviath_core::blueprint::InteractionStyle::FreeText,
            options: vec![],
            directives: std::collections::HashMap::new(),
            abort_options: Vec::new(),
            edit_options: Vec::new(),
        }
    }

    fn make_multiple_choice_point(
        name: &str,
        prompt: &str,
        options: Vec<String>,
    ) -> leviath_core::blueprint::InteractionPoint {
        leviath_core::blueprint::InteractionPoint {
            name: name.to_string(),
            prompt: prompt.to_string(),
            required: false,
            style: leviath_core::blueprint::InteractionStyle::MultipleChoice,
            options,
            directives: std::collections::HashMap::new(),
            abort_options: Vec::new(),
            edit_options: Vec::new(),
        }
    }

    fn make_multiple_choice_point_with_directives(
        name: &str,
        prompt: &str,
        options: Vec<String>,
        directives: std::collections::HashMap<String, String>,
    ) -> leviath_core::blueprint::InteractionPoint {
        leviath_core::blueprint::InteractionPoint {
            name: name.to_string(),
            prompt: prompt.to_string(),
            required: false,
            style: leviath_core::blueprint::InteractionStyle::MultipleChoice,
            options,
            directives,
            abort_options: Vec::new(),
            edit_options: Vec::new(),
        }
    }

    fn make_multiple_choice_point_with_abort(
        name: &str,
        prompt: &str,
        options: Vec<String>,
        abort_options: Vec<String>,
    ) -> leviath_core::blueprint::InteractionPoint {
        leviath_core::blueprint::InteractionPoint {
            name: name.to_string(),
            prompt: prompt.to_string(),
            required: false,
            style: leviath_core::blueprint::InteractionStyle::MultipleChoice,
            options,
            directives: std::collections::HashMap::new(),
            abort_options,
            edit_options: Vec::new(),
        }
    }

    fn make_multiple_choice_point_with_edit(
        name: &str,
        prompt: &str,
        options: Vec<String>,
        edit_options: Vec<String>,
    ) -> leviath_core::blueprint::InteractionPoint {
        leviath_core::blueprint::InteractionPoint {
            name: name.to_string(),
            prompt: prompt.to_string(),
            required: false,
            style: leviath_core::blueprint::InteractionStyle::MultipleChoice,
            options,
            directives: std::collections::HashMap::new(),
            abort_options: Vec::new(),
            edit_options,
        }
    }

    fn make_confirm_point(name: &str, prompt: &str) -> leviath_core::blueprint::InteractionPoint {
        leviath_core::blueprint::InteractionPoint {
            name: name.to_string(),
            prompt: prompt.to_string(),
            required: false,
            style: leviath_core::blueprint::InteractionStyle::Confirm,
            options: vec![],
            directives: std::collections::HashMap::new(),
            abort_options: Vec::new(),
            edit_options: Vec::new(),
        }
    }

    /// Clean up stale test-* run directories from previous test runs.
    /// Helper: spawn a background task that watches for pending.json and writes a response.
    /// Returns a JoinHandle that should be awaited or aborted after the test.
    fn spawn_interaction_responder(
        run_id: String,
        responses: Vec<crate::interaction::InteractionResponse>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut resp_iter = responses.into_iter();
            let mut last_req_id = String::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if let Some(req) = crate::interaction::read_request(&run_id) {
                    // Skip if this is the same request we already responded to
                    // (waiting for the main task to consume it)
                    if req.id == last_req_id {
                        continue;
                    }
                    if let Some(mut resp) = resp_iter.next() {
                        last_req_id = req.id.clone();
                        resp.request_id = req.id.clone();
                        crate::interaction::write_response(&run_id, &resp).unwrap();
                    } else {
                        break;
                    }
                }
            }
        })
    }

    #[tokio::test]
    async fn interactive_points_single_free_text_point_stdin() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_single_free_text_point_stdin",
        );
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent answer");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-ft-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_free_text_point("feedback", "What do you think?")];

        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![InteractionResponse::text("", "my feedback")],
        );

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_points_multiple_choice_stdin() {
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("interactive_points_multiple_choice_stdin");
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-mc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_multiple_choice_point(
            "pick_one",
            "Choose an option",
            vec!["Option A".to_string(), "Option B".to_string()],
        )];

        // choice_index 1 = "Option B" (0-based)
        let responder =
            spawn_interaction_responder(run_id.clone(), vec![InteractionResponse::choice("", 1)]);

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    // ─── Regression: a choice with a configured followup must ask for ─────
    // elaboration and loop back to re-prompt the same point, instead of the
    // chosen option's bare label (e.g. "Revise") being the only thing that
    // ever reaches the model.
    #[tokio::test]
    async fn interactive_points_directive_option_stays_in_stage_and_injects_directive() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_directive_option_stays_in_stage_and_injects_directive",
        );
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-mc-followup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let mut directives = std::collections::HashMap::new();
        directives.insert(
            "Revise".to_string(),
            "Call ask_user_text to learn what to change, then re-plan.".to_string(),
        );
        let points = vec![make_multiple_choice_point_with_directives(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Revise".to_string()],
            directives,
        )];

        // Round 1: pick "Revise" (choice_index 1) → must inject the directive
        // and loop back IN-STAGE (no second free-text request is issued — the
        // agent drives the next step itself). Round 2: pick "Approve"
        // (choice_index 0, no directive) → the point loop ends.
        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![
                InteractionResponse::choice("", 1),
                InteractionResponse::choice("", 0),
            ],
        );

        let outcome = run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            8,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await
        .unwrap();

        responder.abort();

        // Completed (not aborted) after the revision loop.
        assert_eq!(outcome, PointsOutcome::Completed);

        // The directive text must have been injected into the agent's context,
        // proving the run stayed in-stage and told the agent what to do next.
        let window = engine
            .world()
            .get::<leviath_runtime::ContextWindow>(entity)
            .unwrap();
        let conversation = window.get_region("conversation").unwrap();
        let all_content: String = conversation
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_contains_display(
            all_content.contains("directive: Call ask_user_text"),
            "expected the injected directive in context, got",
            &all_content,
        );
        assert!(all_content.contains("Revise"));
        assert!(all_content.contains("Approve"));

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_points_abort_option_returns_aborted() {
        // Selecting an abort option must short-circuit the stage with
        // PointsOutcome::Aborted — no directive, no fall-through — so the
        // executor can cancel the run deterministically.
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_abort_option_returns_aborted",
        );
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-abort-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_multiple_choice_point_with_abort(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Abort".to_string()],
            vec!["Abort".to_string()],
        )];

        // Pick "Abort" (choice_index 1).
        let responder =
            spawn_interaction_responder(run_id.clone(), vec![InteractionResponse::choice("", 1)]);

        let outcome = run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            8,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await
        .unwrap();

        responder.abort();

        assert_eq!(outcome, PointsOutcome::Aborted);

        // The abort short-circuits before the option label is injected, so the
        // conversation region must NOT carry an "Abort" acknowledgement line.
        let window = engine
            .world()
            .get::<leviath_runtime::ContextWindow>(entity)
            .unwrap();
        let conversation = window.get_region("conversation").unwrap();
        let all_content: String = conversation
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!all_content.contains("User [plan_approval]: Abort"));

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_points_edit_ipc_request_write_error() {
        // Covers the `request_interaction_async(...).await?` error arm in the
        // IPC edit path: the responder answers the choice, then blocks the
        // atomic write's tmp target with a directory. Reading the
        // (already-written) choice response still works, but the subsequent
        // edit request's write_request fails, so the `?` propagates and the
        // stage returns Err. Blocking the tmp path fails the write on every
        // platform.
        use crate::interaction::InteractionResponse;

        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_edit_ipc_request_write_error",
        );
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-edit-werr-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_multiple_choice_point_with_edit(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Add detail".to_string()],
            vec!["Add detail".to_string()],
        )];

        let rid = run_id.clone();
        let responder = tokio::spawn(async move {
            // The stage posts its (choice) request before awaiting a response,
            // so it is present once this task is scheduled.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let req = crate::interaction::read_request(&rid)
                .expect("stage posts its interaction request before awaiting a response");
            let mut resp = InteractionResponse::choice("", 1); // "Add detail" (edit)
            resp.request_id = req.id.clone();
            crate::interaction::write_response(&rid, &resp).unwrap();
            // Block the next atomic write: the choice response above is still
            // readable, but the edit request's write_request writes to
            // `pending.json.tmp`, which we replace with a *directory* so that
            // write fails on every platform.
            let dir = runstate::run_dir(&rid);
            std::fs::create_dir_all(dir.join("pending.json.tmp")).unwrap();
        });

        let result = run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            8,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await;

        let _ = responder.await;
        let dir = runstate::run_dir(&run_id);
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn interactive_points_edit_option_ipc_path() {
        // Edit option over the background file-IPC path: the engine issues an
        // EditText request, the user's edited text is injected, then the point
        // is re-presented. (The foreground path is covered separately.)
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("interactive_points_edit_option_ipc_path");
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-edit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_multiple_choice_point_with_edit(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Add detail".to_string()],
            vec!["Add detail".to_string()],
        )];

        // Round 1: "Add detail" (index 1) → engine issues EditText → user
        // submits edited text → Round 2: "Approve" (index 0) → complete.
        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![
                InteractionResponse::choice("", 1),
                InteractionResponse::text("", "EDITED VIA IPC"),
                InteractionResponse::choice("", 0),
            ],
        );

        let outcome = run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            8,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await
        .unwrap();

        responder.abort();
        assert_eq!(outcome, PointsOutcome::Completed);

        let window = engine
            .world()
            .get::<leviath_runtime::ContextWindow>(entity)
            .unwrap();
        let conversation = window.get_region("conversation").unwrap();
        let all_content: String = conversation
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_content.contains("edited the output directly"));
        assert!(all_content.contains("EDITED VIA IPC"));

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_points_foreground_abort_option_returns_aborted() {
        // Same deterministic abort on the foreground (stdin) path.
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let points = vec![make_multiple_choice_point_with_abort(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Abort".to_string()],
            vec!["Abort".to_string()],
        )];

        let ask_foreground =
            move |_req: &crate::interaction::InteractionRequest| InteractionResponse::choice("", 1);

        let outcome = run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            8,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();

        assert_eq!(outcome, PointsOutcome::Aborted);
    }

    #[tokio::test]
    async fn interactive_points_edit_option_opens_editor_seeded_with_output() {
        // Selecting an edit option must DETERMINISTICALLY open an EditText
        // interaction seeded with the stage's last output (the plan), inject
        // the edited text, and re-present the point — no dependence on the
        // model calling an edit tool.
        use crate::interaction::{InteractionKind, InteractionResponse};
        use std::sync::Mutex;

        let bp = make_blueprint(vec![make_stage("main")]);
        // Mock provider returns "Agent response" → becomes the edit seed.
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let points = vec![make_multiple_choice_point_with_edit(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Add detail".to_string()],
            vec!["Add detail".to_string()],
        )];

        // Records the body of the EditText request the engine issued.
        let seen_edit_body: Mutex<Option<String>> = Mutex::new(None);
        let choice_round = Mutex::new(0usize);
        let ask_foreground = |req: &crate::interaction::InteractionRequest| match req.kind {
            InteractionKind::EditText => {
                *seen_edit_body.lock().unwrap() = req.body.clone();
                InteractionResponse::text(&req.id, "EDITED PLAN")
            }
            _ => {
                // Round 1: "Add detail" (index 1). Round 2: "Approve" (index 0).
                let mut r = choice_round.lock().unwrap();
                let idx = if *r == 0 { 1 } else { 0 };
                *r += 1;
                InteractionResponse::choice(&req.id, idx)
            }
        };

        let outcome = run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            8,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();

        assert_eq!(outcome, PointsOutcome::Completed);
        // The editor was seeded with the stage's last output (the plan).
        assert_eq!(
            seen_edit_body.lock().unwrap().as_deref(),
            Some("Agent response")
        );

        // The edited text was injected into the agent's context.
        let window = engine
            .world()
            .get::<leviath_runtime::ContextWindow>(entity)
            .unwrap();
        let conversation = window.get_region("conversation").unwrap();
        let all_content: String = conversation
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_contains_display(
            all_content.contains("edited the output directly")
                && all_content.contains("EDITED PLAN"),
            "expected the edited text injected into context, got",
            &all_content,
        );
    }

    // ─── run_interactive_points_stage: real foreground (stdin) path ─────────
    //
    // Unlike every test above (all of which pass `Some((run_id, meta))` and
    // go through the background file-IPC responder, regardless of test name),
    // these call `run_interactive_points_stage_with` directly with
    // `run_context: None` and a mock `ask_foreground` closure -- the actual
    // foreground/stdin dispatch path. In production the binary injects a real
    // stdin asker (via `StageCallbacks::interaction_point_asker`); tests use a
    // mock so this branch never blocks on real stdin.

    /// Shared mock foreground asker — a plain `fn` matching
    /// [`super::executor::InteractionAsker`] — for the public
    /// `run_interactive_points_stage` wrapper tests. Approves the first choice.
    fn wrapper_mock_asker(
        _req: &crate::interaction::InteractionRequest,
    ) -> crate::interaction::InteractionResponse {
        crate::interaction::InteractionResponse::choice("", 0)
    }

    #[test]
    fn ipc_mode_unused_asker_returns_empty_text() {
        // Covers the background-mode placeholder asker's body (never invoked by
        // the interaction loop itself, since background resolves via IPC).
        let req = crate::interaction::InteractionRequest::free_text("x", "p", "s", false);
        let resp = super::ipc_mode_unused_asker(&req);
        assert_eq!(resp.request_id, "x");
        assert_eq!(resp.value.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn public_wrapper_foreground_dispatches_via_injected_asker() {
        // Drives the public `run_interactive_points_stage` on the foreground
        // path (run_context None) with an injected `Some(asker)` and a real
        // interaction point -- covering the wrapper's `Some(asker)` arm and the
        // foreground stdin-dispatch branch reached through the public API.
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();
        let points = vec![make_multiple_choice_point(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Revise".to_string()],
        )];

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            Some(wrapper_mock_asker),
        )
        .await
        .unwrap();

        let window = engine
            .world()
            .get::<leviath_runtime::ContextWindow>(entity)
            .unwrap();
        let conversation = window.get_region("conversation").unwrap();
        let all_content: String = conversation
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_content.contains("Approve"));
    }

    #[tokio::test]
    async fn public_wrapper_foreground_without_asker_errors() {
        // Covers the wrapper's bail arm: foreground (run_context None) with
        // interaction points but no injected asker is a misconfiguration.
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();
        let points = vec![make_free_text_point("clarify", "Anything else?")];

        let result = run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            None,
        )
        .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("require an interaction asker"));
    }

    #[tokio::test]
    async fn foreground_path_choice_no_followup_completes_without_ipc() {
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let points = vec![make_multiple_choice_point(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Revise".to_string()],
        )];

        let ask_foreground = |_req: &crate::interaction::InteractionRequest| {
            InteractionResponse::choice("", 0) // "Approve" -- no followup configured
        };

        // run_context: None exercises the `(None, None)` arm and the whole
        // foreground branch, with no real run directory or IPC responder
        // needed at all.
        run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();

        let window = engine
            .world()
            .get::<leviath_runtime::ContextWindow>(entity)
            .unwrap();
        let conversation = window.get_region("conversation").unwrap();
        let all_content: String = conversation
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_content.contains("Approve"));
    }

    #[tokio::test]
    async fn foreground_path_free_text_point_completes_without_ipc() {
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let points = vec![make_free_text_point("clarify", "Anything else to add?")];
        let ask_foreground = |_req: &crate::interaction::InteractionRequest| {
            InteractionResponse::text("", "nothing else")
        };

        run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();

        let window = engine
            .world()
            .get::<leviath_runtime::ContextWindow>(entity)
            .unwrap();
        let conversation = window.get_region("conversation").unwrap();
        let all_content: String = conversation
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_content.contains("nothing else"));
    }

    #[tokio::test]
    async fn foreground_path_directive_option_stays_in_stage_and_injects_directive() {
        use crate::interaction::InteractionResponse;
        use std::collections::VecDeque;
        use std::sync::Mutex;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let mut directives = std::collections::HashMap::new();
        directives.insert(
            "Revise".to_string(),
            "Call ask_user_text to learn what to change.".to_string(),
        );
        let points = vec![make_multiple_choice_point_with_directives(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Revise".to_string()],
            directives,
        )];

        // Round 1: "Revise" (index 1) -> injects the directive and loops back
        // IN-STAGE (no second free-text ask). Round 2: "Approve" (index 0, no
        // directive) -> the point loop ends. So `ask_foreground` is called
        // exactly twice — proving no engine-issued elaboration prompt.
        let queued = Mutex::new(VecDeque::from(vec![
            InteractionResponse::choice("", 1),
            InteractionResponse::choice("", 0),
        ]));
        let ask_foreground =
            move |_req: &crate::interaction::InteractionRequest| {
                queued.lock().unwrap().pop_front().expect(
                    "ask_foreground called more times than expected (no elaboration prompt)",
                )
            };

        let outcome = run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            8,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();

        assert_eq!(outcome, PointsOutcome::Completed);

        let window = engine
            .world()
            .get::<leviath_runtime::ContextWindow>(entity)
            .unwrap();
        let conversation = window.get_region("conversation").unwrap();
        let all_content: String = conversation
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_contains_display(
            all_content.contains("directive: Call ask_user_text"),
            "expected the injected directive in context, got",
            &all_content,
        );
        assert!(all_content.contains("Revise"));
        assert!(all_content.contains("Approve"));
    }

    #[tokio::test]
    async fn foreground_path_revision_cap_stops_infinite_revise_loop() {
        use crate::interaction::InteractionResponse;
        use std::collections::VecDeque;
        use std::sync::Mutex;

        // A user who always picks a directive option ("Revise") must eventually
        // be cut off by MAX_REVISION_ROUNDS (4) rather than looping forever.
        // `max_iterations = 1` makes the per-segment iteration budget 0, so the
        // `remaining_iterations == 0` guard never fires and ONLY the
        // `revision_round + 1 >= MAX_REVISION_ROUNDS` guard can break the loop.
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let mut directives = std::collections::HashMap::new();
        directives.insert(
            "Revise".to_string(),
            "Call ask_user_text to learn what to change.".to_string(),
        );
        let points = vec![make_multiple_choice_point_with_directives(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Revise".to_string()],
            directives,
        )];

        // "Revise" every round. The cap breaks the loop after exactly 4 asks;
        // a 5th `pop_front` would panic on the empty queue.
        let queued = Mutex::new(VecDeque::from(vec![
            InteractionResponse::choice("", 1),
            InteractionResponse::choice("", 1),
            InteractionResponse::choice("", 1),
            InteractionResponse::choice("", 1),
        ]));
        let ask_foreground = move |_req: &crate::interaction::InteractionRequest| {
            queued
                .lock()
                .unwrap()
                .pop_front()
                .expect("ask_foreground called more times than the revision cap allows")
        };

        let outcome = run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            1,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();

        // Terminated normally (not aborted) after the cap — no infinite loop.
        assert_eq!(outcome, PointsOutcome::Completed);
    }

    #[tokio::test]
    async fn interactive_points_confirm_stdin() {
        let _guard = crate::runstate::isolate_runs_dir_for_test("interactive_points_confirm_stdin");
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-cf-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_confirm_point("confirm_step", "Are you sure?")];

        let responder =
            spawn_interaction_responder(run_id.clone(), vec![InteractionResponse::text("", "yes")]);

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_points_multiple_points_all_visited() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_multiple_points_all_visited",
        );
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Mid-stage response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-mp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![
            make_free_text_point("step1", "Tell me about step 1"),
            make_multiple_choice_point("step2", "Pick one", vec!["A".to_string(), "B".to_string()]),
            make_confirm_point("step3", "Confirm?"),
        ];

        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![
                InteractionResponse::text("", "first input"),
                InteractionResponse::choice("", 0),
                InteractionResponse::text("", "yes"),
            ],
        );

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            9,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_points_with_zero_remaining_iterations() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_with_zero_remaining_iterations",
        );
        use crate::interaction::InteractionResponse;

        // max_iterations = 1, points = 2 → iterations_per_segment rounds down to 0
        // The stage should still run (just skips inference) and ask interaction points
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-zero-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![
            make_free_text_point("p1", "Point 1"),
            make_free_text_point("p2", "Point 2"),
        ];

        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![
                InteractionResponse::text("", "a"),
                InteractionResponse::text("", "b"),
            ],
        );

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            1, // small max_iterations
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_points_empty_user_input_is_ok() {
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("interactive_points_empty_user_input_is_ok");
        use crate::interaction::InteractionResponse;

        // Empty answer → nothing injected into context window (branch coverage)
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_free_text_point("ask", "Say something")];

        // Respond with empty text
        let responder =
            spawn_interaction_responder(run_id.clone(), vec![InteractionResponse::text("", "")]);

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            2,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn autonomous_stage_with_tools() {
        // Test the autonomous stage with tools provided (exercises tool executor path)
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Tool result response");
        let mut io = MockIO::new();

        let tools = vec![leviath_providers::Tool {
            name: "my_tool".to_string(),
            description: "a test tool".to_string(),
            parameters: serde_json::json!({}),
        }];

        run_autonomous_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            1,
            &tools,
            None,
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        let all_output = io.outputs.join("");
        assert_contains_debug(
            all_output.contains("Tool result response"),
            "Expected tool response in",
            &io.outputs,
        );
    }

    // ─── Coverage: tools branch inference + context-window update (lines 76-112) ─

    /// A `RunIO` impl whose `get_user_input` always returns `""` so the
    /// interactive loop exits after one turn.  Used to drive the `has_tools=true`
    /// branch of `run_interactive_stage` all the way through lines 76-112.
    struct EmptyInputIO;

    #[async_trait]
    impl crate::commands::run::io::RunIO for EmptyInputIO {
        async fn on_stage_enter(
            &mut self,
            _stage: &leviath_core::Stage,
            _visit_num: usize,
            _provider: &str,
            _model: &str,
        ) {
        }
        async fn on_stage_complete(
            &mut self,
            _stage_name: &str,
            _result: &leviath_core::blueprint::StageResult,
            _next_stage: Option<&str>,
        ) {
        }
        async fn on_output(&mut self, _text: &str) {}
        async fn on_tokens(&mut self, _prompt: usize, _completion: usize, _cached: usize) {}
        async fn on_tool_call(&mut self, _tool_name: &str, _tool_id: &str, _result: &str) {}
        async fn get_user_input(&mut self, _prompt: &str) -> Option<String> {
            Some("".to_string())
        }
        async fn on_error(&mut self, _error: &str) {}
        async fn on_provider_missing(&mut self, _provider: &str) {}
        fn is_background(&self) -> bool {
            false
        }
        fn write_context_snapshot(&mut self, _snapshot: &crate::runstate::RegionSnapshot) {}
    }

    #[tokio::test]
    async fn interactive_stage_tools_foreground_completes_one_turn() {
        // Exercises the `has_tools = true` branch of `run_interactive_stage`:
        //   • run_inference_loop_filtered succeeds → lines 76-112 (including the
        //     `if let Some(mut window)` at line 105 whose `}` at line 112 was a gap)
        //   • foreground path for user input → empty → loop breaks at line 177
        // No meta/run_id → takes the foreground input path (line 172).
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = EmptyInputIO;

        let tools = vec![leviath_providers::Tool {
            name: "t".to_string(),
            description: "t".to_string(),
            parameters: serde_json::json!({}),
        }];

        let result = run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            5,
            &tools,
            None, // foreground: no run_id → uses io.get_user_input()
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn empty_input_io_all_methods_are_covered() {
        // Directly exercise every method on EmptyInputIO so LLVM sees them as covered.
        let mut io = EmptyInputIO;
        let stage = make_stage("main");
        let result_val = leviath_core::blueprint::StageResult::Success;
        io.on_stage_enter(&stage, 0, "p", "m").await;
        io.on_stage_complete("main", &result_val, None).await;
        io.on_output("text").await;
        io.on_tokens(1, 2, 3).await;
        io.on_tool_call("tool", "id", "result").await;
        let _ = io.get_user_input("prompt").await;
        io.on_error("err").await;
        io.on_provider_missing("prov").await;
        assert!(!io.is_background());
        let snap = crate::runstate::RegionSnapshot {
            name: "snap".to_string(),
            kind: "pinned".to_string(),
            current_tokens: 0,
            max_tokens: 0,
            entries: vec![],
        };
        io.write_context_snapshot(&snap);
    }

    #[tokio::test]
    async fn interactive_stage_with_tools_max_turns_reached() {
        // Test that the interactive stage tool-path also respects max_iterations=0
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new();

        let tools = vec![leviath_providers::Tool {
            name: "test_tool".to_string(),
            description: "test".to_string(),
            parameters: serde_json::json!({}),
        }];

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            0, // immediately hits limit
            &tools,
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert_contains_debug(
            io.outputs.iter().any(|o| o.contains("[Max turns reached]")),
            "Expected max turns message",
            &io.outputs,
        );
    }

    // ─── Coverage: entities without ContextWindow ────────────────────────────
    // Lines 455:17 and 485:17 require an entity with no ContextWindow.

    /// Helper: make an engine with the mock provider but spawn a bare entity
    /// (no ContextWindow). Used for the "no-window" else-branch coverage in
    /// interactive_points (lines 455 and 485).
    fn make_engine_bare_entity(
        provider_content: &str,
    ) -> (leviath_runtime::AgentEngine, bevy_ecs::prelude::Entity) {
        let mut registry = leviath_runtime::ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::new(provider_content)),
        );
        let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
        let entity = engine.world_mut().spawn(()).id();
        (engine, entity)
    }

    // ─── Coverage: tool-path inference error (line 74) ───────────────────────

    #[tokio::test]
    async fn interactive_stage_tools_inference_error_propagates() {
        // Covers line 74: `?` on `run_inference_loop_filtered` when has_tools=true
        // and the provider doesn't exist → returns Err.
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "unused");
        let mut io = MockIO::new();

        let tools = vec![leviath_providers::Tool {
            name: "t".to_string(),
            description: "t".to_string(),
            parameters: serde_json::json!({}),
        }];

        let result = run_interactive_stage(
            &mut engine,
            entity,
            "nonexistent-provider", // causes inference to fail
            "test-model",
            5,
            &tools,
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await;

        assert!(result.is_err());
    }

    // ─── Coverage: both-fail inference (lines 128:42, 128:84) ───────────────

    /// A provider where both `infer_stream` and `infer` always fail.
    struct AlwaysFailingProvider;

    #[async_trait]
    impl Provider for AlwaysFailingProvider {
        async fn infer(&self, _r: InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            Err(ProviderError::Other("infer always fails".to_string()))
        }

        async fn infer_stream(
            &self,
            _r: InferenceRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<leviath_providers::StreamChunk, ProviderError>,
                        > + Send,
                >,
            >,
            ProviderError,
        > {
            Err(ProviderError::Other("stream always fails".to_string()))
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "always-fail"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }
    }

    // Exercises the rarely-called Provider trait methods on AlwaysFailingProvider
    // so that LLVM marks them as covered.
    #[test]
    fn always_failing_provider_trait_methods_are_covered() {
        let p = AlwaysFailingProvider;
        assert_eq!(p.name(), "always-fail");
        assert_eq!(p.count_tokens("hello world", "any"), 2);
        assert_eq!(p.max_context_tokens("any"), 100_000);
        let caps = p.capabilities("any");
        let _ = caps;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let models = rt.block_on(p.list_models()).unwrap();
        assert!(models.is_empty());
    }

    #[tokio::test]
    async fn interactive_stage_fallback_inference_also_fails_propagates() {
        // Covers lines 128:42 and 128:84: `?` on `run_inference_filtered` in the
        // streaming-unavailable fallback path when fallback also fails.
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity_with_provider(
            &bp,
            "always-fail",
            Arc::new(AlwaysFailingProvider),
        );
        let mut io = MockIO::new();

        let result = run_interactive_stage(
            &mut engine,
            entity,
            "always-fail",
            "test-model",
            5,
            &[], // no tools → streaming path → falls back → also fails
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await;

        assert!(result.is_err());
    }

    // ─── Coverage: request_interaction_async error (lines 168:80) ────────────

    #[tokio::test]
    async fn interactive_stage_tools_request_interaction_async_error() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_stage_tools_request_interaction_async_error",
        );
        // Covers the `?` on `request_interaction_async`: write_request's atomic
        // write targets `pending.json.tmp`, which we replace with a *directory*
        // so the write fails and the Err propagates -- on every platform.
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-rdonly-tools-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        // Create the run directory, then block the atomic write's tmp target
        // with a directory so write_request fails.
        runstate::create_run(&meta).unwrap();
        let dir = runstate::run_dir(&run_id);
        std::fs::create_dir_all(dir.join("pending.json.tmp")).unwrap();

        let tools = vec![leviath_providers::Tool {
            name: "t".to_string(),
            description: "t".to_string(),
            parameters: serde_json::json!({}),
        }];

        let result = run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            5,
            &tools,
            Some((&run_id, &mut meta)),
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await;

        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn interactive_stage_no_tools_request_interaction_async_error() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_stage_no_tools_request_interaction_async_error",
        );
        // Same as above but no-tools path (streaming). Covers the same `?` via
        // the else branch (no tools).
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-rdonly-notools-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();
        let dir = runstate::run_dir(&run_id);
        std::fs::create_dir_all(dir.join("pending.json.tmp")).unwrap();

        let result = run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            5,
            &[], // no tools → streaming path
            Some((&run_id, &mut meta)),
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await;

        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
    }

    // ─── Coverage: interactive_points None run_context (lines 374:21, 388:17) ─

    #[tokio::test]
    async fn interactive_points_foreground_empty_content_no_meta() {
        // Covers line 374:21: `if let (Some(run_id), Some(ref m))` is false when
        // run_context=None. Also covers line 388:17: `if let Some(ref mut m)` is
        // None (no meta_holder).
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "content from agent");
        let mut io = MockIO::new();

        let points = vec![make_free_text_point("ask", "Tell me something")];

        let ask_foreground = |_req: &crate::interaction::InteractionRequest| {
            InteractionResponse::text("", "user reply")
        };

        run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            None, // run_context = None → no meta_holder, no run_id_owned
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();

        let all_output = io.outputs.join("");
        assert!(all_output.contains("content from agent"));
    }

    // ─── Coverage: unwrap_or_else closure (lines 421:96, 444:48, 444:65) ────
    // The fallback is called when choice_index is out of range for the options slice.

    #[tokio::test]
    async fn interactive_points_out_of_range_choice_falls_back_to_text_foreground() {
        // Covers lines 444:48, 444:65: foreground path, choice_index out of range →
        // `unwrap_or_else` closure called → `response_as_text` returns the text value.
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let points = vec![make_multiple_choice_point(
            "pick",
            "Choose",
            vec!["A".to_string(), "B".to_string()],
        )];

        // choice_index 99 is out of range → response_as_choice returns None →
        // unwrap_or_else fires → response_as_text("fallback")
        let ask_foreground = |_req: &crate::interaction::InteractionRequest| {
            let mut resp = InteractionResponse::choice("", 99);
            resp.value = Some("fallback".to_string());
            resp
        };

        run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();
        // Success = unwrap_or_else path ran without panic
    }

    #[tokio::test]
    async fn interactive_points_out_of_range_choice_falls_back_to_text_background() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_out_of_range_choice_falls_back_to_text_background",
        );
        // Covers line 421:96: background (IPC) path, choice_index out of range →
        // unwrap_or_else closure called.
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-oob-bg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_multiple_choice_point(
            "pick",
            "Choose",
            vec!["X".to_string(), "Y".to_string()],
        )];

        let mut oob_resp = InteractionResponse::choice("", 99);
        oob_resp.value = Some("fallback-text".to_string());
        let responder = spawn_interaction_responder(run_id.clone(), vec![oob_resp]);

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    // ─── Coverage: empty user_text with no ContextWindow (line 455:17) ───────

    #[tokio::test]
    async fn interactive_points_empty_user_text_no_context_window() {
        // Covers line 455:17: `if !user_text.is_empty()` is false AND entity has no
        // ContextWindow → outer if block is skipped entirely.
        use crate::interaction::InteractionResponse;

        let (mut engine, entity) = make_engine_bare_entity("Agent response");
        let mut io = MockIO::new();

        let points = vec![make_free_text_point("ask", "Say something")];

        let ask_foreground = |_req: &crate::interaction::InteractionRequest| {
            InteractionResponse::text("", "") // empty response
        };

        run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();
    }

    // ─── Coverage: directive injection with no ContextWindow ────────────────

    #[tokio::test]
    async fn interactive_points_directive_no_context_window() {
        // Covers the `None` branch of the directive-injection `if let Some(mut
        // window) = ...get_mut::<ContextWindow>` — a bare entity has no
        // ContextWindow, so the directive can't be injected but the loop must
        // still proceed and complete without panicking.
        use crate::interaction::InteractionResponse;
        use std::collections::VecDeque;
        use std::sync::Mutex;

        let (mut engine, entity) = make_engine_bare_entity("Agent response");
        let mut io = MockIO::new();

        let mut directives = std::collections::HashMap::new();
        directives.insert("Revise".to_string(), "Ask what to change.".to_string());
        let points = vec![make_multiple_choice_point_with_directives(
            "plan",
            "Pick",
            vec!["Approve".to_string(), "Revise".to_string()],
            directives,
        )];

        // Round 1: "Revise" → directive path (no ContextWindow to inject into).
        // Round 2: "Approve" → exits.
        let queued = Mutex::new(VecDeque::from(vec![
            InteractionResponse::choice("", 1),
            InteractionResponse::choice("", 0),
        ]));
        let ask_foreground = move |_req: &crate::interaction::InteractionRequest| {
            queued
                .lock()
                .unwrap()
                .pop_front()
                .expect("ask_foreground called more times than expected")
        };

        let outcome = run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            12,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();
        assert_eq!(outcome, PointsOutcome::Completed);
    }

    #[tokio::test]
    async fn interactive_points_edit_option_no_context_window() {
        // Covers the `None` branch of the edit-injection `if let Some(mut
        // window) = ...get_mut::<ContextWindow>`: a bare entity (no
        // ContextWindow) drives the foreground edit path, so the edited text
        // can't be injected but the loop still re-presents and completes.
        use crate::interaction::InteractionResponse;
        use std::collections::VecDeque;
        use std::sync::Mutex;

        let (mut engine, entity) = make_engine_bare_entity("Agent response");
        let mut io = MockIO::new();

        let points = vec![make_multiple_choice_point_with_edit(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Add detail".to_string()],
            vec!["Add detail".to_string()],
        )];

        // Round 1: "Add detail" (edit option) → foreground editor returns
        // non-empty edited text (no window to inject into). Round 2: "Approve".
        let queued = Mutex::new(VecDeque::from(vec![
            InteractionResponse::choice("", 1),
            InteractionResponse::text("", "EDITED WITHOUT WINDOW"),
            InteractionResponse::choice("", 0),
        ]));
        let ask_foreground = move |_req: &crate::interaction::InteractionRequest| {
            queued
                .lock()
                .unwrap()
                .pop_front()
                .expect("ask_foreground called more times than expected")
        };

        let outcome = run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            12,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();
        assert_eq!(outcome, PointsOutcome::Completed);
    }

    // ─── Coverage: final segment None run_context (line 513:13) ─────────────

    #[tokio::test]
    async fn interactive_points_final_segment_no_run_context() {
        // Covers line 513:13: `if let (Some(run_id), Some(ref m))` is false in the
        // final-segment block (run_context = None).
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "final segment content");
        let mut io = MockIO::new();

        let points = vec![make_free_text_point("ask", "Tell me something")];

        let ask_foreground = |_req: &crate::interaction::InteractionRequest| {
            InteractionResponse::text("", "user reply")
        };

        run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            8,
            &[],
            None,
            &points,
            None, // run_context = None → hits the else at line 513
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();

        let all_output = io.outputs.join("");
        assert!(all_output.contains("final segment content"));
    }

    // ─── Coverage: final segment Err (line 527:9) ───────────────────────────

    #[tokio::test]
    async fn interactive_points_final_segment_inference_error_skipped() {
        // Covers line 527:9: `if let Ok(resp)` is Err in the final segment.
        // Nonexistent provider → run_inference_loop_filtered returns Err →
        // the `if let Ok(resp)` arm is not taken (Err is silently ignored).
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "unused");
        let mut io = MockIO::new();

        let points = vec![make_free_text_point("ask", "Tell me something")];

        let ask_foreground = |_req: &crate::interaction::InteractionRequest| {
            InteractionResponse::text("", "user reply")
        };

        let result = run_interactive_points_stage_with(
            &mut engine,
            entity,
            "nonexistent-provider-final",
            "test-model",
            8,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await;

        // Err is silently dropped by `if let Ok(resp)` so function returns Ok
        assert!(result.is_ok());
    }

    // ─── Coverage: spawn_interaction_responder exhaustion (lines 1382:25, 1384:17, 1386:9) ─

    #[tokio::test]
    async fn spawn_interaction_responder_exhausts_and_exits_loop() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "spawn_interaction_responder_exhausts_and_exits_loop",
        );
        // Covers lines 1382:25, 1384:17, 1386:9: the `else { break; }` branch in
        // spawn_interaction_responder when resp_iter is exhausted.
        //
        // We provide 1 response for 2 interaction points. The responder handles
        // point-1, exhausts its iterator, then when point-2's request appears it
        // takes the `else { break }` path. The stage will be waiting on point-2
        // forever — we use a timeout to avoid blocking.
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-responder-exhaust-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![
            make_free_text_point("p1", "First question"),
            make_free_text_point("p2", "Second question"),
        ];

        // 1 response for 2 points → iterator exhausted when p2 arrives
        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![InteractionResponse::text("", "answer to p1")],
        );

        let mut exec_binding = noop_exec;
        let stage_fut = run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut exec_binding,
            None,
        );
        // Stage will block on p2 forever; we cancel it after a timeout
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), stage_fut).await;

        // The responder should have broken out of its loop when p2 arrived
        let responder_result =
            tokio::time::timeout(std::time::Duration::from_secs(5), responder).await;
        assert!(responder_result.is_ok());

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    // ─── Coverage: spawn_interaction_responder None arm ─────────────────────────
    //
    // Forces the `if let Some(req) = read_request()` to return None at least once
    // so the closing `}` of that if-let is counted by LLVM. We spawn the responder
    // with no responses, sleep long enough for it to poll (and get None), then abort.

    #[tokio::test]
    async fn spawn_interaction_responder_polls_none_then_aborts() {
        // Spawn a responder with no responses and no pending request.
        // The responder will sleep 50ms and then call read_request → None.
        // We wait 80ms to guarantee at least one poll occurred, then abort.
        let run_id = format!(
            "test-responder-none-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );

        // No run directory → read_request returns None immediately
        let responder = spawn_interaction_responder(run_id.clone(), vec![]);

        // Wait long enough for at least one poll (sleep is 50ms → wait 80ms)
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        responder.abort();
        // Test just verifies coverage of the None arm — no assertion needed
    }

    // ─── Coverage: request_interaction_async error in interactive_points (line 474:97) ─

    #[tokio::test]
    async fn interactive_points_request_interaction_async_error_propagates() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_request_interaction_async_error_propagates",
        );
        // Covers the `?` on `request_interaction_async` in the background path.
        //
        // We respond to the first choice with "Revise" (which has a directive),
        // then immediately block the atomic write's tmp target with a directory
        // so:
        //   1) response.json is readable (already written)
        //   2) the directive is injected and the loop re-runs inference
        //   3) round 2's choice request → write_request fails → Err via `?`
        // Blocking the tmp path fails on every platform.
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-followup-err-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let mut directives = std::collections::HashMap::new();
        directives.insert("Revise".to_string(), "Ask what to change.".to_string());
        let points = vec![make_multiple_choice_point_with_directives(
            "plan",
            "Pick",
            vec!["Approve".to_string(), "Revise".to_string()],
            directives,
        )];

        let run_id_for_responder = run_id.clone();
        let responder: tokio::task::JoinHandle<()> = tokio::spawn(async move {
            // Poll immediately — request not yet written (main task hasn't yielded
            // to run_interactive_points_stage yet), so this returns None on the
            // first iteration. This exercises the while-loop-body (true) path.
            while crate::interaction::read_request(&run_id_for_responder).is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            // Now read_request returned Some — retrieve and handle the request.
            let req = crate::interaction::read_request(&run_id_for_responder).unwrap();
            let mut resp = InteractionResponse::choice("", 1); // "Revise"
            resp.request_id = req.id.clone();
            crate::interaction::write_response(&run_id_for_responder, &resp).unwrap();
            // Immediately block the next atomic write: response.json is on disk
            // so take_response can still read it, but the followup's
            // write_request writes `pending.json.tmp`, which we replace with a
            // directory so that write fails.
            let dir = runstate::run_dir(&run_id_for_responder);
            let _ = std::fs::create_dir_all(dir.join("pending.json.tmp"));
        });
        // Yield so the spawned task runs its first read_request check (returns None)
        // before we start run_interactive_points_stage (which writes the request).
        tokio::task::yield_now().await;

        let result = run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            8,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await;

        let _ = responder.await;

        let dir = runstate::run_dir(&run_id);
        let _ = std::fs::remove_dir_all(&dir);

        // The error from the failed write_request propagates via `?`.
        assert!(result.is_err());
    }

    // ─── Coverage: empty inference content with run context (lines 374:21, 513:13) ─

    #[tokio::test]
    async fn interactive_points_empty_content_with_run_context() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_empty_content_with_run_context",
        );
        // Covers lines 374:21 and 513:13: `}` of `if !resp.content.is_empty()` when
        // the provider returns empty content ("") AND run_context is Some.
        // The if-block is skipped (false branch), producing the uncovered segment.
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        // MockProvider with empty string → resp.content.is_empty() == true
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-empty-content-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_free_text_point("q", "Any thoughts?")];
        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![InteractionResponse::text("", "my answer")],
        );

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));

        // When content is empty, io.on_output was NOT called
        assert!(io.outputs.is_empty());
    }

    // ─── Coverage: first request_interaction_async fails (line 421:96) ─────────

    #[tokio::test]
    async fn interactive_points_first_request_interaction_async_error() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_first_request_interaction_async_error",
        );
        // Covers the `?` on `request_interaction_async` for the initial
        // (non-followup) interaction point when `write_request` fails because
        // the atomic write's tmp target is blocked before the stage starts.

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let run_id = format!(
            "test-ip-first-req-err-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        // Block the atomic write's tmp target BEFORE calling the stage so that
        // `write_request` (called by `request_interaction_async`) fails
        // immediately → propagates via `?`. A directory at `pending.json.tmp`
        // makes the write fail on every platform.
        let dir = runstate::run_dir(&run_id);
        std::fs::create_dir_all(dir.join("pending.json.tmp")).unwrap();

        let points = vec![make_free_text_point("q", "What now?")];

        let result = run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
            None,
        )
        .await;

        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
    }

    // ─── normalize_for_followup + lookup_directive tests ──────────────────

    #[test]
    fn normalize_for_followup_preserves_simple_text() {
        assert_eq!(super::normalize_for_followup("Approve"), "Approve");
    }

    #[test]
    fn normalize_for_followup_normalizes_em_dash() {
        assert_eq!(
            super::normalize_for_followup("Revise \u{2014} I'll describe changes"),
            "Revise - I'll describe changes"
        );
    }

    #[test]
    fn normalize_for_followup_normalizes_en_dash() {
        assert_eq!(
            super::normalize_for_followup("Revise \u{2013} changes"),
            "Revise - changes"
        );
    }

    #[test]
    fn normalize_for_followup_collapses_whitespace() {
        assert_eq!(
            super::normalize_for_followup("Revise  —  I'll   describe"),
            "Revise - I'll describe"
        );
    }

    #[test]
    fn lookup_directive_exact_match() {
        let mut directives = std::collections::HashMap::new();
        directives.insert(
            "Revise \u{2014} I'll describe changes".to_string(),
            "Ask what to change.".to_string(),
        );
        let result = super::lookup_directive(&directives, "Revise \u{2014} I'll describe changes");
        assert_eq!(result, Some("Ask what to change."));
    }

    #[test]
    fn lookup_directive_normalized_match_em_dash_vs_hyphen() {
        let mut directives = std::collections::HashMap::new();
        directives.insert(
            "Revise \u{2014} I'll describe changes".to_string(),
            "Ask what to change.".to_string(),
        );
        // User text uses ASCII hyphen instead of em dash
        let result = super::lookup_directive(&directives, "Revise - I'll describe changes");
        assert_eq!(result, Some("Ask what to change."));
    }

    #[test]
    fn lookup_directive_normalized_match_extra_whitespace() {
        let mut directives = std::collections::HashMap::new();
        directives.insert(
            "Revise \u{2014} I'll describe changes".to_string(),
            "Ask what to change.".to_string(),
        );
        // User text has extra whitespace and an en dash
        let result =
            super::lookup_directive(&directives, "Revise  \u{2013}  I'll  describe  changes");
        assert_eq!(result, Some("Ask what to change."));
    }

    #[test]
    fn lookup_directive_no_match() {
        let mut directives = std::collections::HashMap::new();
        directives.insert("Revise".to_string(), "prompt".to_string());
        let result = super::lookup_directive(&directives, "Approve");
        assert_eq!(result, None);
    }

    #[test]
    fn is_abort_option_exact_and_normalized() {
        let aborts = vec!["Abort \u{2014} cancel this run".to_string()];
        // Exact match
        assert!(super::is_abort_option(
            &aborts,
            "Abort \u{2014} cancel this run"
        ));
        // Normalized (ASCII hyphen + extra whitespace)
        assert!(super::is_abort_option(
            &aborts,
            "Abort  -  cancel  this  run"
        ));
        // Non-abort option
        assert!(!super::is_abort_option(&aborts, "Approve"));
        // Empty abort list
        assert!(!super::is_abort_option(&[], "Abort"));
    }

    #[tokio::test]
    async fn foreground_edit_option_breaks_when_iteration_budget_exhausted() {
        // An edit option selected once the per-point iteration budget is spent
        // hits the `remaining_iterations == 0` guard in the edit branch and
        // breaks out of the point loop.
        use crate::interaction::{InteractionKind, InteractionResponse};

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let points = vec![make_multiple_choice_point_with_edit(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Edit".to_string()],
            vec!["Edit".to_string()],
        )];

        // Always pick "Edit" (index 1) and always return non-empty edited text.
        // max_iterations = 2 with one point (2 segments) gives 1 iteration per
        // segment, so remaining hits 0 by the second round's guard check.
        let ask_foreground = |req: &crate::interaction::InteractionRequest| match req.kind {
            InteractionKind::EditText => InteractionResponse::text(&req.id, "edited text"),
            _ => InteractionResponse::choice(&req.id, 1),
        };

        let outcome = run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            2,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();

        assert_eq!(outcome, PointsOutcome::Completed);
    }

    #[tokio::test]
    async fn foreground_edit_option_empty_edit_is_skipped_and_no_directive_completes() {
        // Round 1: pick the edit option but submit an EMPTY edit → the
        // `if !edited.is_empty()` block is skipped. Round 2: pick a plain option
        // with no directive → the no-directive `else` branch completes the
        // stage.
        use crate::interaction::{InteractionKind, InteractionResponse};
        use std::sync::Mutex;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let points = vec![make_multiple_choice_point_with_edit(
            "plan_approval",
            "Approve the plan?",
            vec!["Approve".to_string(), "Edit".to_string()],
            vec!["Edit".to_string()],
        )];

        let choice_round = Mutex::new(0usize);
        let ask_foreground = |req: &crate::interaction::InteractionRequest| match req.kind {
            // Empty edit → the inject block is skipped.
            InteractionKind::EditText => InteractionResponse::text(&req.id, ""),
            _ => {
                // Round 1: "Edit" (index 1). Round 2: "Approve" (index 0, no
                // directive → completes).
                let mut r = choice_round.lock().unwrap();
                let idx = if *r == 0 { 1 } else { 0 };
                *r += 1;
                InteractionResponse::choice(&req.id, idx)
            }
        };

        let outcome = run_interactive_points_stage_with(
            &mut engine,
            entity,
            "mock",
            "test-model",
            8,
            &[],
            None,
            &points,
            None,
            &mut io,
            &mut noop_exec,
            &ask_foreground,
        )
        .await
        .unwrap();

        assert_eq!(outcome, PointsOutcome::Completed);
    }
}
