//! What an accepted `submit_output` means for the stage that made it.
//!
//! A stage used to end only on a reply with no tool call, so a model that
//! calls a tool on every turn kept handing back the answer it had already
//! handed back, until the stage's iteration cap. Two rules stop that:
//!
//! - A stage with nowhere else to go ends on an accepted submission: no normal
//!   edge leaves it, or it is the stage a fan-out worker was started to run.
//!   The submission is its last word, and no further model call is made.
//! - Anywhere else, the answer the stage already submitted, submitted again,
//!   is not recorded again. The model is told it is in and to reply without a
//!   tool call, and after [`MAX_REPEATED_SUBMISSIONS`] repeats the stage ends.
//!
//! Ending goes through the stage's ordinary transition, so its gates, its
//! review and its required regions still have their say: a gate that sends the
//! stage back gets a stage that can submit again.
//!
//! An interactive stage is left alone. Its conversation, not its answer, says
//! when it is over.

use super::*;
use crate::output_tool::{OutputContext, handle_output_tool};
use crate::spec::graph::{EdgeCondition, StageDef};
use crate::spec::run_spec::RunSpec;
use leviath_core::output::{Artifact, FinalOutput};

/// How many times a stage may submit the answer it already submitted before
/// it ends with that answer.
pub(crate) const MAX_REPEATED_SUBMISSIONS: u32 = 2;

/// What a model reads when it submits the answer it already submitted.
pub(crate) const REPEATED_SUBMISSION: &str = "Already recorded: this is the answer you \
     submitted before, so nothing was recorded again. Your stage is complete: reply without a \
     tool call.";

/// The answer the current visit to a stage holds, kept between its batches:
/// the visit it belongs to, and how many times the same answer has come back
/// since it was recorded.
///
/// Written after a batch is past any gate prompt, so a batch held for one and
/// run again is judged against what came before it, never against itself.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisitSubmissions {
    /// The visit the answer was recorded in.
    pub visit: String,
    /// Submissions of the same answer since it was recorded.
    pub repeats: u32,
}

/// The stage ends once this batch's tool results land, unless one of them
/// failed. Set by dispatch on a batch with calls for the tool lane, read by
/// `collect_tools`.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct EndAfterBatch;

/// Whether stage `index` of `spec` ends as soon as an answer is accepted: it
/// is not interactive, and either no edge leaves it on a normal ending or it is
/// the stage this run was started to run as a fan-out worker.
pub(crate) fn ends_on_submission(spec: &RunSpec, index: usize) -> bool {
    let graph = &spec.graph;
    let stage = &graph.stages[index];
    let leaves = graph
        .edges_from(stage.name.as_str())
        .any(|e| matches!(e.when, EdgeCondition::Always | EdgeCondition::LlmChoice));
    let worker = spec.placement.worker_stage.as_ref() == Some(&stage.name);
    stage.mode.interactive().is_none() && (!leaves || worker)
}

/// What one batch's `submit_output` calls came to.
pub(super) struct BatchSubmissions<'a> {
    /// The answer this visit already held when the batch arrived.
    held: Option<&'a FinalOutput>,
    /// Whether a repeated answer is answered as one: every stage but an
    /// interactive one.
    guarded: bool,
    /// The answer this batch recorded, if it recorded one.
    pub(super) submitted: Option<FinalOutput>,
    /// Files an accepted submission produced, by the execution that produced
    /// them.
    pub(super) produced: Vec<(String, Vec<Artifact>)>,
    /// Calls made once the visit already held an answer.
    resubmissions: usize,
    /// Submissions of the same answer since it was recorded.
    repeats: u32,
    /// Whether a call in this batch handed back an answer: one recorded, or
    /// the one already held, again.
    answered: bool,
}

impl<'a> BatchSubmissions<'a> {
    /// The batch's starting point: the answer the agent holds (`recorded`)
    /// counts as this visit's only when `kept` says it was recorded in this
    /// visit. A batch run again after a gate prompt (`rerun`) is judged as it
    /// was the first time, before its own submission was committed.
    pub(super) fn new(
        stage: Option<&StageDef>,
        visit: &str,
        recorded: Option<&'a crate::persistence::FinalOutput>,
        kept: Option<&VisitSubmissions>,
        rerun: bool,
    ) -> Self {
        let kept = kept.filter(|k| k.visit == visit && !rerun);
        Self {
            held: kept.and(recorded).map(|o| &o.0),
            guarded: stage.is_some_and(|s| s.mode.interactive().is_none()),
            submitted: None,
            produced: Vec::new(),
            resubmissions: 0,
            repeats: kept.map_or(0, |k| k.repeats),
            answered: false,
        }
    }

    /// Apply one `submit_output` call, answering what the model reads.
    pub(super) fn apply(
        &mut self,
        call: &crate::components::ToolCall,
        execution: Option<&String>,
        ctx: &OutputContext<'_>,
        window: &mut ContextWindow,
    ) -> String {
        if let Some(previous) = self.submitted.as_ref().or(self.held) {
            self.resubmissions += 1;
            if self.guarded && crate::output_tool::repeats(&call.arguments, previous, ctx.workdir) {
                self.repeats += 1;
                self.answered = true;
                return REPEATED_SUBMISSION.to_string();
            }
        }
        let now = chrono::Utc::now().timestamp();
        let (text, output) = handle_output_tool(&call.arguments, ctx, now, window);
        // A refused submission leaves any earlier one alone: a bad correction
        // must not erase a good answer.
        if let Some(output) = output {
            // Recorded against this call while it is still in hand.
            // `output.json` keeps only the latest answer's files and says
            // nothing about which call made any of them, so a submission a
            // later one replaces would otherwise leave no trace.
            if !output.artifacts.is_empty() {
                self.produced.push((
                    execution.cloned().unwrap_or_default(),
                    output.artifacts.clone(),
                ));
            }
            self.submitted = Some(output);
            self.repeats = 0;
            self.answered = true;
        }
        text
    }

    /// Keep what the batch came to, for the visit's next batch and for the
    /// stage's record, and say whether the stage ends with this batch: an
    /// answer handed back in a stage that [`ends_on_submission`] (`terminal`),
    /// recorded or the held one again, or the same answer submitted
    /// [`MAX_REPEATED_SUBMISSIONS`] times anywhere else.
    pub(super) fn settle(
        &self,
        agent: &mut EntityCommands,
        state: &AgentState,
        terminal: bool,
        ledger: Option<&mut StageLedger>,
    ) -> bool {
        if self.submitted.is_some() || self.held.is_some() {
            agent.insert(VisitSubmissions {
                visit: state.current_visit.clone(),
                repeats: self.repeats,
            });
        }
        if let Some(record) =
            ledger.and_then(|l| l.0.iter_mut().find(|r| r.name == state.current_stage))
        {
            record.resubmissions += self.resubmissions;
        }
        (self.answered && terminal) || (self.guarded && self.repeats >= MAX_REPEATED_SUBMISSIONS)
    }
}

#[cfg(test)]
#[path = "submission_tests.rs"]
mod tests;
