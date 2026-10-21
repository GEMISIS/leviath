//! Asking a stage's validator tool about a submission before the batch that
//! made it is applied.
//!
//! `submit_output` is answered inline by `dispatch_tools`, but a validator
//! tool is a real tool call: an MCP server or a script that may take seconds.
//! It cannot run inside the tick, which drives every other run in the daemon,
//! so it goes out on the tool lane like a stage's refreshing seeds do (see
//! `crate::stage_seeds`) and lands on a later tick.
//!
//! # Holding the batch back
//!
//! A batch whose `submit_output` the tool has to judge is not applied until
//! the verdict is in. [`validate_first`] takes `ReadyForTools` away, sends the
//! calls, and holds the batch on [`PendingValidation`]; [`apply_validation`]
//! files the verdicts on [`ValidatorVerdicts`] and puts `ReadyForTools` back,
//! and the batch is dispatched again with the verdicts in hand. Nothing in the
//! batch has run when it is held, so dispatching it again runs each call once.
//!
//! # Checks, in order
//!
//! Only a submission that passed every cheaper check is sent: the format, the
//! schema and the Rhai validator (`output_tool::precheck`). One that fails
//! them is refused without a tool call, and one that repeats the answer the
//! visit already holds is answered as a repeat. The artifacts are checked
//! once the verdict is in, so a refused answer writes nothing.
//!
//! # What the tool may do
//!
//! Nothing a mid-run call could not. The call is decided by the tool service
//! like any other (its `tool_permissions`, the launch's `allow`, the yolo
//! profile) and the taint gate looks at it as it would at the model's own
//! call. Nobody is asked about it, because nobody is there mid-submission: a
//! call that would ask is a validator that could not run, and so is one the
//! gate blocks. Spawn refuses a validator tool set to `deny` or `ask`, so this
//! is only ever a policy that changed under a resumed run, or taint.
//!
//! # A restart
//!
//! The held batch is the turn's batch, still waiting to be dispatched, so the
//! run's state records it as one. A run resumed from there dispatches it
//! again, and the validation is asked for again: a validator tool must be
//! read-only and idempotent.

use std::collections::HashMap;

use bevy_ecs::prelude::*;
use leviath_scripting::output_validator::Verdict;

use super::submission::BatchSubmissions;
use super::{ReadyForTools, StageInference};
use crate::output_tool::OutputContext;

/// One validator call on the lane, and the submission it judges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidationSite {
    /// The `submit_output` call the verdict is for.
    pub call_id: String,
    /// The id the lane answers the validator call under.
    pub lane_id: String,
    /// The validator tool.
    pub tool: String,
}

/// A run whose batch is held while its validator tool judges a submission.
#[derive(Component, Debug, Clone)]
pub(crate) struct PendingValidation {
    /// The model call whose batch is held.
    pub attempt: String,
    /// The calls out on the lane.
    pub sites: Vec<ValidationSite>,
    /// Verdicts the batch already had, kept with the new ones.
    pub known: HashMap<String, Verdict>,
}

/// What the validator tool said about each submission in a batch, by the
/// `submit_output` call it judged.
#[derive(Component, Debug, Clone, Default)]
pub(crate) struct ValidatorVerdicts {
    /// The model call whose batch they belong to. Verdicts for any other are
    /// not this batch's.
    pub attempt: String,
    /// Each verdict, by call id.
    pub verdicts: HashMap<String, Verdict>,
}

/// Whether a batch can be applied now or waits for its validator tool.
pub(crate) enum Validation {
    /// The batch is held: its validator calls are on the lane.
    Hold,
    /// The batch goes ahead, with these verdicts by call id.
    Ready(HashMap<String, Verdict>),
}

/// The `submit_output` calls of a batch, and what they are judged against.
pub(super) struct Candidates<'a> {
    /// The batch's calls, as the model made them.
    pub calls: &'a [crate::components::ToolCall],
    /// The stage's advertised tools.
    pub stage: &'a StageInference,
    /// What the batch finished before a restart.
    pub recovered: &'a [crate::tool_bridge::ToolResult],
    /// The calls a person already turned down at the taint gate.
    pub resolved: Option<&'a crate::gate_prompt::GateResolved>,
    /// The answer the visit already holds.
    pub submissions: &'a BatchSubmissions<'a>,
    /// What a submission is judged against.
    pub ctx: &'a OutputContext<'a>,
}

impl Candidates<'_> {
    /// Each `submit_output` call the validator tool has to judge, with its
    /// content: one that reaches the output tool (offered, with arguments
    /// that read and meet its schema, not already done or turned down), is
    /// not the held answer again, and passes every cheaper check.
    pub(crate) fn wanting(&self) -> Vec<(String, String)> {
        self.calls
            .iter()
            .filter(|c| crate::output_tool::is_output_tool(&c.name))
            .filter(|c| !self.recovered.iter().any(|(id, _)| *id == c.tool_id))
            .filter(|c| super::tools::unoffered_tool_refusal(self.stage, &c.name).is_none())
            .filter(|c| !c.arguments.is_string())
            .filter(|c| {
                super::tools::invalid_args_refusal(self.stage, &c.name, &c.arguments).is_none()
            })
            .filter(|c| {
                !self
                    .resolved
                    .is_some_and(|r| r.denied.contains_key(&c.tool_id))
            })
            .filter(|c| {
                !self
                    .submissions
                    .repeats_held(&c.arguments, self.ctx.workdir)
            })
            .filter_map(|c| {
                crate::output_tool::precheck(&c.arguments, self.ctx)
                    .ok()
                    .map(|content| (c.tool_id.clone(), content.to_string()))
            })
            .collect()
    }
}

/// Every stage name in the run's graph, for the routing-token guard.
pub(crate) fn stage_names(spec: Option<&crate::insert::RunSpecC>) -> Vec<String> {
    spec.map(|spec| {
        spec.0
            .graph
            .stages
            .iter()
            .map(|s| s.name.to_string())
            .collect()
    })
    .unwrap_or_default()
}

/// What the taint gate is consulted with about a validator call.
pub(crate) struct GateLook<'a> {
    /// The run making the call.
    pub agent_id: &'a str,
    /// The stage it is in.
    pub stage: usize,
    /// Its context, whose taint the call would carry.
    pub window: &'a crate::components::ContextWindow,
    /// The allowlist.
    pub policy: &'a leviath_core::PolicyConfig,
    /// The scripted rules, when there are any.
    pub rules: Option<&'a crate::taint::ScriptRuleChecker>,
    /// `--yolo`: a block is waived, and recorded as waived, as it is for any
    /// call the run makes.
    pub waived: bool,
}

/// The taint gate's word on a validator call to `tool`: nothing when it lets
/// the call through (or there is no gate), and the block's text when it does
/// not. A block is never put to a person.
pub(crate) fn gate_word(
    gate: Option<&mut crate::taint::TaintGate>,
    look: &GateLook<'_>,
    tool: &str,
) -> Option<String> {
    let gate = gate?;
    gate.at_stage(look.stage);
    let decision = gate.check_with_policy(
        look.agent_id,
        tool,
        look.window,
        None,
        look.policy,
        look.rules,
    );
    let (taint, clearance) = decision.blocked_levels()?;
    if look.waived {
        gate.record_allow(
            look.agent_id,
            tool,
            taint,
            clearance,
            leviath_core::taint::GateDecisionSource::YoloAutoApprove,
        );
        return None;
    }
    Some(super::taint_block_message(&decision))
}

/// The lane id a validator call for `call_id` goes out under: prefixed, so
/// it cannot collide with a provider-minted one.
pub(crate) fn lane_id(call_id: &str) -> String {
    format!("validator-{call_id}")
}

/// Send the validator tool every submission in the batch it has yet to
/// judge, and hold the batch until the verdicts land. A batch with nothing to
/// judge, or whose verdicts are all in (`kept`, for the same model call), goes
/// ahead.
///
/// `gate` is the taint gate's word on the tool: the block's text when it
/// blocks, which is the verdict for every submission without a call.
pub(super) fn validate_first(
    entity: Entity,
    attempt: &str,
    candidates: &Candidates<'_>,
    kept: Option<&ValidatorVerdicts>,
    gate: &mut dyn FnMut(&str) -> Option<String>,
    commands: &mut Commands,
) -> Validation {
    let mut verdicts = kept
        .filter(|k| k.attempt == attempt)
        .map(|k| k.verdicts.clone())
        .unwrap_or_default();
    let Some(tool) = candidates.ctx.spec.and_then(|s| s.validator_tool.as_ref()) else {
        return Validation::Ready(verdicts);
    };
    let wanted: Vec<(String, String)> = candidates
        .wanting()
        .into_iter()
        .filter(|(id, _)| !verdicts.contains_key(id))
        .collect();
    if wanted.is_empty() {
        return Validation::Ready(verdicts);
    }
    if let Some(blocked) = gate(&tool.tool) {
        for (id, _) in wanted {
            verdicts.insert(id, Verdict::Unusable(format!("{}: {blocked}", tool.tool)));
        }
        return Validation::Ready(verdicts);
    }
    let sites: Vec<ValidationSite> = wanted
        .iter()
        .map(|(id, _)| ValidationSite {
            call_id: id.clone(),
            lane_id: lane_id(id),
            tool: tool.tool.clone(),
        })
        .collect();
    let calls = wanted
        .iter()
        .zip(&sites)
        .map(|((_, content), site)| leviath_providers::ToolCall {
            id: site.lane_id.clone(),
            name: tool.tool.clone(),
            arguments: tool.arguments(content),
            thought_signature: None,
        })
        .collect();
    tracing::debug!(
        tool = %tool.tool,
        calls = sites.len(),
        "holding a batch while its validator tool judges the submission"
    );
    commands
        .entity(entity)
        .remove::<ReadyForTools>()
        .insert(super::lane_batch::PendingBatch::validation(calls))
        .insert(PendingValidation {
            attempt: attempt.to_string(),
            sites,
            known: verdicts,
        });
    Validation::Hold
}

/// What a validator tool's answer says: `{"ok": true}` passes,
/// `{"ok": false, "reason": "..."}` refuses with the reason. A failed or
/// refused call, no answer at all, and anything that is not such an object
/// is a validator that could not run.
pub(crate) fn verdict_of(tool: &str, answer: Option<&str>) -> Verdict {
    let Some(text) = answer else {
        return Verdict::Unusable(format!("{tool}: the call never answered"));
    };
    if super::call_had_no_effect(text) {
        return Verdict::Unusable(format!("{tool}: {}", super::one_line(text, 500)));
    }
    let ok = serde_json::from_str::<serde_json::Value>(text.trim())
        .ok()
        .and_then(|v| {
            let ok = v.get("ok")?.as_bool()?;
            Some((
                ok,
                v.get("reason").and_then(|r| r.as_str()).map(str::to_string),
            ))
        });
    match ok {
        Some((true, _)) => Verdict::Valid,
        Some((false, Some(reason))) if !reason.trim().is_empty() => Verdict::Invalid(reason),
        Some((false, _)) => Verdict::Invalid(format!("{tool} refused it without saying why")),
        None => Verdict::Unusable(format!(
            "{tool}: the answer is not a verdict (want {{\"ok\": true}} or {{\"ok\": false, \
             \"reason\": \"...\"}}): {}",
            super::one_line(text, 200)
        )),
    }
}

/// File a landed batch of validator calls as verdicts, and release the
/// batch they held.
pub(crate) fn apply_validation(
    entity: Entity,
    pending: &PendingValidation,
    results: &[crate::tool_bridge::ToolResult],
    commands: &mut Commands,
) {
    let mut verdicts = pending.known.clone();
    for site in &pending.sites {
        let answer = results
            .iter()
            .find(|(id, _)| *id == site.lane_id)
            .map(|(_, r)| r.as_str());
        verdicts.insert(site.call_id.clone(), verdict_of(&site.tool, answer));
    }
    commands
        .entity(entity)
        .remove::<PendingValidation>()
        .insert(ValidatorVerdicts {
            attempt: pending.attempt.clone(),
            verdicts,
        })
        .insert(ReadyForTools);
}

#[cfg(test)]
#[path = "validator_tool_tests.rs"]
mod tests;
