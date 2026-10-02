//! The tool-approval prompt lane.
//!
//! A call whose tool policy is `ask` is put to a person before its batch runs
//! (see [`crate::pipeline::lane_batch`]). The question goes out on a task of
//! its own, through the agent's backend on the [`InteractionHub`], and the
//! answer comes back here as it was given. What it means is decided in the
//! world by [`collect_approvals`]: an approval remembers its grant on the
//! run's [`ToolGrants`] at the scope the person chose and charges the run's
//! [`WriteLedger`]; a refusal or a prompt nobody answered becomes the call's
//! result. The batch then carries on deciding the calls after it.
//!
//! [`InteractionHub`]: crate::interaction_hub::InteractionHub

use std::sync::Arc;

use bevy_ecs::prelude::*;
use leviath_core::interaction::{InteractionRequest, InteractionResponse};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::dynamic_interaction::InteractionBackend;
use crate::interaction_hub::{InteractionHub, PromptLane};
use crate::pipeline::lane_batch::{AwaitingApproval, PendingBatch};
use crate::pipeline::tool_verdicts::{Decision, ToolGrants, WriteLedger};

/// A call to put to a person for approval.
pub(crate) struct ApprovalAsk {
    /// The agent whose batch holds the call.
    pub entity: Entity,
    /// That agent's run id, for the hub's per-agent backend.
    pub agent_id: String,
    /// The call.
    pub call: leviath_providers::ToolCall,
    /// The stage it was made in, which the prompt names.
    pub stage: String,
    /// What an approval for the stage or the run is remembered under.
    pub keys: Vec<String>,
}

/// A person's answer to an approval prompt, as they gave it.
pub(crate) struct ApprovalOutcome {
    /// The agent whose batch holds the call.
    pub entity: Entity,
    /// The call's id.
    pub call_id: String,
    /// The answer.
    pub response: InteractionResponse,
    /// The hub's prompt timeout, when one is configured: the prompt that
    /// closed unanswered says so.
    pub timeout_secs: Option<u64>,
}

/// The sending side of the approval lane, and where its prompts run.
#[derive(Resource)]
pub(crate) struct ApprovalStage {
    /// Where answers are reported.
    outcomes: UnboundedSender<ApprovalOutcome>,
    /// Wakes the tick loop when one lands.
    wake: Arc<Notify>,
    /// The runtime prompts are asked on.
    runtime: Handle,
}

/// The receiving side of the approval lane.
#[derive(Resource)]
pub(crate) struct ApprovalResults(UnboundedReceiver<ApprovalOutcome>);

/// Install the approval lane in `world`, asking on `runtime`.
pub(crate) fn install(world: &mut World, runtime: Handle, wake: Arc<Notify>) {
    let (outcomes, results) = tokio::sync::mpsc::unbounded_channel();
    world.insert_resource(ApprovalStage {
        outcomes,
        wake,
        runtime,
    });
    world.insert_resource(ApprovalResults(results));
}

/// Put `ask` to a person, on a task of its own.
pub(crate) fn ask(lane: (&InteractionHub, &ApprovalStage), ask: ApprovalAsk) {
    let (hub, stage) = lane;
    stage.runtime.spawn(run_approval_prompt(
        ask,
        PromptLane {
            hub: hub.clone(),
            outcomes: stage.outcomes.clone(),
            wake: stage.wake.clone(),
        },
    ));
}

/// Ask a person to approve a call and report the answer as given.
async fn run_approval_prompt(ask: ApprovalAsk, lane: PromptLane<ApprovalOutcome>) {
    let ApprovalAsk {
        entity,
        agent_id,
        call,
        stage,
        keys,
    } = ask;
    let backend = lane.hub.backend_for(agent_id);
    let request = InteractionRequest::tool_approval(
        backend.request_id("approve"),
        &call.name,
        call.arguments,
        &stage,
        &keys,
    );
    let response = backend.ask(request).await;
    let _ = lane.outcomes.send(ApprovalOutcome {
        entity,
        call_id: call.id,
        response,
        timeout_secs: backend.timeout_secs(),
    });
    lane.wake.notify_one();
}

/// The tool result an approval prompt that resolved with no answer gives: the
/// prompt was cancelled, or a configured `[limits] interaction_timeout_secs`
/// ran out. The timeout is named only when there is one; with none set a
/// prompt cannot expire, and blaming a timeout would send the operator looking
/// for a setting that does not exist in their config.
pub fn unanswered_approval_result(tool: &str, timeout_secs: Option<u64>) -> String {
    match timeout_secs {
        Some(secs) => format!(
            "[denied] no one answered the approval prompt for '{tool}' before the \
             interaction timeout ({secs} s, `[limits] interaction_timeout_secs`); \
             the call did not run. Answer prompts in `lev dash`, raise the \
             timeout, or set this tool to \"allow\" for the stage."
        ),
        None => format!(
            "[denied] the approval prompt for '{tool}' was closed without an answer; \
             the call did not run. Answer prompts in `lev dash`, or set this tool \
             to \"allow\" for the stage."
        ),
    }
}

/// The tool result a declined approval gives the model.
///
/// Without feedback it is the exact sentence it has always been (tests and
/// docs quote it). With feedback the person's words follow a `Feedback:`
/// marker on the same line, so the model reads the redirect as part of the
/// refusal rather than as a stray user message somewhere later in the context.
pub fn declined_result(tool: &str, feedback: Option<&str>) -> String {
    match feedback {
        Some(text) => format!("[denied] User declined tool call '{tool}'. Feedback: {text}"),
        None => format!("[denied] User declined tool call '{tool}'."),
    }
}

/// What `collect_approvals` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type ApprovalQuery = (
    &'static mut PendingBatch,
    &'static AwaitingApproval,
    Option<&'static mut ToolGrants>,
    Option<&'static mut WriteLedger>,
);

/// Apply each answered approval to its batch: decide the call, remember an
/// approval's grant and charge, and let the batch carry on deciding.
pub(crate) fn collect_approvals(
    results: Option<ResMut<ApprovalResults>>,
    mut agents: Query<ApprovalQuery>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    // A world with no approval lane asks nobody, so nothing comes back.
    let Some(mut results) = results else {
        return;
    };
    while let Ok(out) = results.0.try_recv() {
        // An answer for a batch that is no longer waiting on it - the agent
        // ended, or its batch went, meanwhile - has nothing left to decide.
        let Ok((mut batch, asked, grants, ledger)) = agents.get_mut(out.entity) else {
            continue;
        };
        crate::tick_scope::enter(out.entity);
        let tool = batch
            .call(&out.call_id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let decision = match out.response.approved {
            Some(true) => {
                if let Some(mut grants) = grants {
                    grants.grant(out.response.scope, &asked.keys);
                }
                if let Some(mut ledger) = ledger {
                    ledger.written = ledger.written.saturating_add(asked.charge);
                }
                Decision::Run
            }
            Some(false) => Decision::Refuse(declined_result(&tool, out.response.deny_feedback())),
            // The hub's neutral answer: the prompt was cancelled, or (only when
            // a timeout is configured) nobody answered it in time. Saying
            // "declined" here would blame a person who never saw the prompt.
            None => {
                let timeout = out.timeout_secs;
                tracing::warn!(
                    tool = %tool,
                    timeout_secs = timeout,
                    "approval prompt resolved unanswered; the call did not run"
                );
                Decision::Refuse(unanswered_approval_result(&tool, timeout))
            }
        };
        batch.decide(out.call_id, decision);
        commands.entity(out.entity).remove::<AwaitingApproval>();
    }
}

#[cfg(test)]
#[path = "approval_prompt_tests.rs"]
mod tests;
