//! The tools that act on a run: `cancel`, `message`, `respond`.

use super::*;

pub(crate) async fn cancel(shared: &Shared, args: &Args) -> CallOutcome {
    let run_id = str_arg(args, "run_id").unwrap_or_default();
    let daemon_error = match shared.daemon_ready().await {
        Ok(()) => {
            let request = ControlRequest::Cancel {
                run_id: run_id.clone(),
            };
            match shared.control.request(&request).await {
                Ok(ControlResponse::Ok { ok: true }) => {
                    return ok(
                        format!("cancelled run {run_id}"),
                        json!({ "run_id": run_id, "cancelled": true }),
                        None,
                    );
                }
                Ok(ControlResponse::Ok { ok: false }) => {
                    return fail(
                        format!("no run '{run_id}' to cancel"),
                        json!({ "run_id": run_id }),
                    );
                }
                Ok(other) => {
                    return fail(
                        format!("unexpected daemon response: {other:?}"),
                        json!({ "run_id": run_id }),
                    );
                }
                Err(e) => e.to_string(),
            }
        }
        Err(e) => e,
    };
    // The daemon is down, wedged, or too busy to answer: record the cancel on
    // disk rather than leave the host with nothing, exactly as `lev cancel`.
    use crate::runstate::ForceCancelOutcome as O;
    let dir = run_dir_in(&shared.env.runs_dir, &run_id);
    match force_cancel_in(&dir, leviath_core::duration::now_secs()) {
        O::Terminated => ok(
            format!(
                "cancelled run {run_id} on disk (the daemon did not answer: {daemon_error}); \
                 restart the daemon so it picks up the change"
            ),
            json!({ "run_id": run_id, "cancelled": true, "on_disk": true }),
            None,
        ),
        O::AlreadyTerminal => ok(
            format!("run {run_id} had already finished; nothing to cancel"),
            json!({ "run_id": run_id, "cancelled": false, "already_finished": true }),
            None,
        ),
        O::NoSuchRun => fail(
            format!(
                "the daemon did not answer ({daemon_error}), and there is no run '{run_id}' on disk"
            ),
            json!({ "run_id": run_id }),
        ),
        O::WriteFailed => fail(
            format!(
                "the daemon did not answer ({daemon_error}), and run {run_id}'s record could not \
                 be rewritten to record the cancel"
            ),
            json!({ "run_id": run_id }),
        ),
    }
}

pub(crate) async fn message(shared: &Shared, args: &Args) -> CallOutcome {
    let run_id = str_arg(args, "run_id").unwrap_or_default();
    let content = str_arg(args, "content").unwrap_or_default();
    let target_region = str_arg(args, "target_region");
    let request = ControlRequest::Message {
        agent_id: run_id.clone(),
        content,
        target_region,
    };
    match bool_request(
        shared,
        request,
        &format!("message delivered to run {run_id}"),
        &format!("run {run_id} is not accepting messages (finished, or its stage takes none)"),
    )
    .await
    {
        Ok(text) => ok(text, json!({ "run_id": run_id, "delivered": true }), None),
        Err(text) => fail(text, json!({ "run_id": run_id, "delivered": false })),
    }
}

/// The answer implied by `respond`'s arguments: approve/deny first, then an
/// explicit choice, otherwise free text (empty when omitted). Same precedence
/// as `lev respond`'s flags.
pub(crate) fn build_interaction_response(
    request_id: &str,
    args: &Args,
) -> Result<InteractionResponse, String> {
    // The schema holds `scope` to its three words; anything else read here
    // is the default.
    let scope = match str_arg(args, "scope").as_deref() {
        Some("stage") => ApprovalScope::Stage,
        Some("session") => ApprovalScope::Run,
        _ => ApprovalScope::Once,
    };
    let feedback = str_arg(args, "feedback");
    if let Some(approved) = bool_arg(args, "approved") {
        return match feedback {
            Some(feedback) if !approved => Ok(InteractionResponse::deny_with_feedback(
                request_id, &feedback,
            )),
            Some(_) => Err(
                "`feedback` goes with `approved: false`: it tells the model what to do \
                            instead of the call"
                    .to_string(),
            ),
            None => Ok(InteractionResponse::approval(request_id, approved, scope)),
        };
    }
    if let Some(index) = uint_arg(args, "choice_index") {
        return Ok(InteractionResponse::choice(request_id, index as usize));
    }
    Ok(InteractionResponse::text(
        request_id,
        str_arg(args, "value").unwrap_or_default(),
    ))
}

pub(crate) async fn respond(shared: &Shared, args: &Args) -> CallOutcome {
    let Some(request_id) = str_arg(args, "request_id") else {
        return list_interactions(shared).await;
    };
    let response = match build_interaction_response(&request_id, args) {
        Ok(response) => response,
        Err(message) => return CallOutcome::InvalidParams(message),
    };
    let request = ControlRequest::AnswerInteraction { response };
    match bool_request(
        shared,
        request,
        &format!("answered interaction {request_id}; call `wait` for the run to continue"),
        &format!("no open interaction '{request_id}'; `respond` without a request_id lists them"),
    )
    .await
    {
        Ok(text) => ok(
            text,
            json!({ "request_id": request_id, "answered": true }),
            None,
        ),
        Err(text) => fail(text, json!({ "request_id": request_id, "answered": false })),
    }
}

async fn list_interactions(shared: &Shared) -> CallOutcome {
    if let Err(e) = shared.daemon_ready().await {
        return fail(
            format!("the leviath daemon is not available: {e}"),
            json!({}),
        );
    }
    match shared
        .control
        .request(&ControlRequest::ListInteractions)
        .await
    {
        Ok(ControlResponse::Interactions { interactions }) => {
            let text = match interactions.is_empty() {
                true => "no open interactions".to_string(),
                false => interactions
                    .iter()
                    .map(|(run_id, req)| {
                        format!(
                            "run {run_id}: request_id={} ({:?} in stage '{}'): {}",
                            req.id, req.kind, req.stage_name, req.prompt
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            let open: Vec<Value> = interactions
                .iter()
                .map(|(run_id, req)| json!({ "run_id": run_id, "request_id": req.id, "interaction": req }))
                .collect();
            ok(text, json!({ "interactions": open }), None)
        }
        Ok(other) => fail(format!("unexpected daemon response: {other:?}"), json!({})),
        Err(e) => fail(
            format!("the leviath daemon is not reachable ({e})"),
            json!({}),
        ),
    }
}
