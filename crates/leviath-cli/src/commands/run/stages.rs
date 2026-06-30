//! Stage runner functions: interactive, autonomous, interactive_points.

use leviath_core::lifecycle::CompactionConfig;
use leviath_runtime::AgentEngine;

use crate::runstate::{self, RunMeta};

use super::helpers::record_stage_log;
use super::helpers::record_stage_output;
use super::inference::stream_inference;

/// Run an interactive stage.
///
/// `run_context`: if `Some((run_id, meta))`, interaction is handled via the
/// file-based IPC channel (background worker). If `None`, stdin is used
/// (foreground).
#[allow(clippy::too_many_arguments)]
pub async fn run_interactive_stage<F, Fut>(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    run_context: Option<(&str, &mut RunMeta)>,
    stage_name: &str,
    executor: &mut F,
) -> anyhow::Result<()>
where
    F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut,
    Fut: std::future::Future<Output = Vec<(String, String)>>,
{
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
            println!("\n[Max turns reached]");
            break;
        }

        if has_tools {
            let per_turn_iters = 10_usize.min(max_iterations.saturating_sub(turn));
            let response = engine
                .run_inference_loop_filtered(
                    entity,
                    provider_name,
                    model_name,
                    tools.to_vec(),
                    per_turn_iters,
                    None,
                    None,
                    None,
                    executor,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Inference error: {}", e))?;

            println!("\nAssistant: {}", response.content);
            let token_line = format!(
                "[Tokens: {} in, {} out]",
                response.tokens_used.prompt_tokens, response.tokens_used.completion_tokens
            );
            println!("\n{}", token_line);

            // Route to per-stage files so the dashboard can display them.
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

            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = response.content.len() / 4 + 1;
                let _ = window.add_to_region(
                    "conversation",
                    format!("Assistant: {}", response.content),
                    tokens,
                );
            }
        } else {
            let response = match stream_inference(engine, entity, provider_name, model_name, None)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("Streaming unavailable, falling back: {}", e);
                    let r = engine
                        .run_inference_filtered(entity, provider_name, model_name, Vec::new(), None)
                        .await
                        .map_err(|e| anyhow::anyhow!("Inference error: {}", e))?;
                    println!("\nAssistant: {}", r.content);
                    r
                }
            };

            println!(
                "\n[Tokens: {} in, {} out]",
                response.tokens_used.prompt_tokens, response.tokens_used.completion_tokens
            );

            if let Some(ref mut m) = meta_holder {
                m.prompt_tokens += response.tokens_used.prompt_tokens;
                m.completion_tokens += response.tokens_used.completion_tokens;
                m.cached_tokens += response.tokens_used.cached_tokens;
                m.touch();
                let _ = runstate::write_meta(m);
            }

            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = response.content.len() / 4 + 1;
                let _ = window.add_to_region(
                    "conversation",
                    format!("Assistant: {}", response.content),
                    tokens,
                );
            }
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
            crate::interaction::request_interaction_stdin(&req);
            // For stdin, we need to actually read in the FreeText path
            use std::io::Write;
            print!("\nYou: ");
            std::io::stdout().flush().ok();
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            buf.trim().to_string()
        };

        if input.is_empty() || input == "/quit" || input == "/exit" {
            println!("\n[Session ended]");
            break;
        }

        if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
            let tokens = input.len() / 4 + 1;
            let _ = window.add_to_region("conversation", format!("User: {}", input), tokens);
        }

        turn += 1;
    }

    Ok(())
}

/// Run an autonomous stage with the real tool executor.
#[allow(clippy::too_many_arguments)]
pub async fn run_autonomous_stage<F, Fut>(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    routing: Option<&leviath_runtime::ToolResultRoutingConfig>,
    compaction_config: Option<&CompactionConfig>,
    executor: &mut F,
) -> anyhow::Result<()>
where
    F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut,
    Fut: std::future::Future<Output = Vec<(String, String)>>,
{
    let response = engine
        .run_inference_loop_filtered(
            entity,
            provider_name,
            model_name,
            tools.to_vec(),
            max_iterations,
            None,
            routing,
            compaction_config,
            executor,
        )
        .await;

    match response {
        Ok(resp) => {
            println!("{}", resp.content);
            println!(
                "\n[Tokens used: {} input, {} output]",
                resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
            );
        }
        Err(e) => {
            println!("Inference error: {}", e);
        }
    }
    Ok(())
}

/// Run an InteractivePoints stage: autonomous iterations with pauses at each interaction point.
///
/// `run_context`: if `Some((run_id, meta))`, interaction is handled via the
/// file-based IPC channel (background worker). If `None`, stdin is used
/// (foreground).
#[allow(clippy::too_many_arguments)]
pub async fn run_interactive_points_stage<F, Fut>(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    routing: Option<&leviath_runtime::ToolResultRoutingConfig>,
    compaction_config: Option<&CompactionConfig>,
    points: &[leviath_core::blueprint::InteractionPoint],
    run_context: Option<(&str, &mut RunMeta)>,
    executor: &mut F,
) -> anyhow::Result<()>
where
    F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut,
    Fut: std::future::Future<Output = Vec<(String, String)>>,
{
    use crate::interaction::{
        make_interaction_id, request_interaction_async, request_interaction_stdin,
        response_as_choice, response_as_text, InteractionRequest,
    };
    use leviath_runtime::ContextWindow;

    if points.is_empty() {
        return run_autonomous_stage(
            engine,
            entity,
            provider_name,
            model_name,
            max_iterations,
            tools,
            routing,
            compaction_config,
            executor,
        )
        .await;
    }

    let (run_id_owned, mut meta_holder): (Option<String>, Option<&mut RunMeta>) = match run_context
    {
        Some((rid, m)) => (Some(rid.to_string()), Some(m)),
        None => (None, None),
    };

    let segments = points.len() + 1;
    let iterations_per_segment = max_iterations / segments;
    let mut remaining_iterations = max_iterations;

    for (pt_idx, point) in points.iter().enumerate() {
        let iters = iterations_per_segment.min(remaining_iterations);
        if iters > 0 {
            let response = engine
                .run_inference_loop_filtered(
                    entity,
                    provider_name,
                    model_name,
                    tools.to_vec(),
                    iters,
                    None,
                    routing,
                    compaction_config,
                    executor,
                )
                .await;

            if let Ok(resp) = response {
                if !resp.content.is_empty() {
                    println!("{}", resp.content);
                    // Route agent response to the per-stage output file so the dashboard can display it
                    if let (Some(run_id), Some(ref m)) = (&run_id_owned, &meta_holder) {
                        record_stage_output(run_id, m.stage_index, &resp.content);
                    }
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
        let req_id = make_interaction_id(pt_idx, 0);
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
                InteractionRequest::free_text(req_id, &point.prompt, &point.name, point.required)
            }
        };

        // Dispatch via file IPC or stdin
        let user_text =
            if let (Some(run_id), Some(ref mut meta)) = (&run_id_owned, &mut meta_holder) {
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
                // Foreground (stdin) path — `request_interaction_stdin` prints and reads
                let resp = request_interaction_stdin(&ipc_req);
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

        if !user_text.is_empty() {
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = user_text.len() / 4 + 1;
                let content = format!("User [{}]: {}", point.name, user_text);
                let _ = window.add_to_region("conversation", content, tokens);
            }
        }
    }

    if remaining_iterations > 0 {
        let response = engine
            .run_inference_loop_filtered(
                entity,
                provider_name,
                model_name,
                tools.to_vec(),
                remaining_iterations,
                None,
                routing,
                compaction_config,
                executor,
            )
            .await;

        if let Ok(resp) = response {
            if !resp.content.is_empty() {
                println!("{}", resp.content);
                if let (Some(run_id), Some(ref m)) = (&run_id_owned, &meta_holder) {
                    record_stage_output(run_id, m.stage_index, &resp.content);
                }
            }
            let token_line = format!(
                "[Tokens used: {} input, {} output]",
                resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
            );
            println!("\n{}", token_line);
            if let (Some(_), Some(ref m)) = (&run_id_owned, &meta_holder) {
                record_stage_log(&m.run_id, m.stage_index, &token_line);
            }
        }
    }

    Ok(())
}
