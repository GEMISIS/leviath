//! What an accepted `submit_output` means for the stage that made it.
//!
//! A model that calls a tool on every turn never sends the reply with no
//! tool call that ends a stage, so without these rules it would hand back the
//! same answer until the stage's iteration cap:
//!
//! - A stage with nowhere else to go ends on an accepted submission: no normal
//!   edge leaves it, or it is the stage a fan-out worker was started to run.
//!   The submission is its last word, and no further model call is made.
//! - Anywhere else, the answer the stage already submitted, submitted again,
//!   is not recorded again. The model is told it is in and to reply without a
//!   tool call, and after [`MAX_REPEATED_SUBMISSIONS`] repeats the stage ends.
//!
//! An answer a validator tool judges is accepted only once its verdict is in
//! (see `validator_tool`), so a stage that ends on it ends after the verdict.
//!
//! Ending goes through the stage's ordinary transition, so its gates, its
//! review and its required regions still have their say: a gate that sends the
//! stage back gets a stage that can submit again.
//!
//! A batch in which any other call failed does not end the stage, whichever
//! rule would have ended it: a refused call (bad arguments, a call cut off, a
//! tool the stage was not offered, a gate's `[blocked]`) and a failed one on the
//! tool lane alike. The model is shown the failure, and the next batch that
//! hands the answer back ends the stage. A shell command that exits non-zero
//! has still run, so it does not count as failed.
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

/// How many characters of the last refusal a failure message carries.
const REFUSAL_CHARS: usize = 500;

/// What a stage's visit has had refused: how many `submit_output` calls its
/// validators turned away, and what the last refusal said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageRefusals {
    /// The visit the refusals belong to; a new visit starts the count again.
    pub visit: String,
    /// Submissions refused.
    pub count: u32,
    /// The last refusal, cut to a sensible length.
    pub last: String,
}

/// The refusals each stage of a run has had, kept so that a stage that ends
/// without an answer can say it was refused rather than that it never tried.
/// Lost with the world on a restart, which then falls back to the plainer
/// wording.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SubmissionRefusals(pub std::collections::BTreeMap<String, StageRefusals>);

impl SubmissionRefusals {
    /// Why a run (or a worker) that owed an answer has none: the refusals of
    /// the first of `stages` that has any, else that no stage ever called
    /// `submit_output`.
    pub(crate) fn reason<'a>(
        refusals: Option<&Self>,
        stages: impl IntoIterator<Item = &'a str>,
    ) -> String {
        let refused =
            refusals.and_then(|r| stages.into_iter().find_map(|s| r.0.get(s).map(|x| (s, x))));
        match refused {
            Some((stage, r)) => format!(
                "stage '{stage}' submitted {} time{}, every submission was refused; \
                 last refusal: {}",
                r.count,
                if r.count == 1 { "" } else { "s" },
                r.last
            ),
            None => "the stage that owes one never called submit_output".to_string(),
        }
    }
}

/// The answer a batch started from, kept on its gate hold so that the batch,
/// run again once the prompts resolve, is judged as it was the first time:
/// by then its own submission has been committed.
#[derive(Debug, Clone, Default)]
pub(crate) struct HeldSubmission {
    /// The answer the visit held.
    pub answer: Option<FinalOutput>,
    /// Submissions of the same answer since it was recorded.
    pub repeats: u32,
}

/// Tell subscribers that a run's answer was recorded: a `submit_output` call
/// accepted, or an output stage's produced parts taken as its answer.
///
/// Nothing for a batch that submitted nothing or had its submission refused,
/// and nothing in a world with no subscribers or no run behind the agent. The
/// content is cut to [`crate::host::MAX_EVENT_OUTPUT_BYTES`] on the frame.
pub(crate) fn announce_submission(
    sink: Option<&crate::host::WorldEventSink>,
    metadata: Option<&RunMetadata>,
    agent_id: &str,
    submitted: Option<FinalOutput>,
) {
    if let (Some(sink), Some(md), Some(output)) = (sink, metadata, submitted) {
        let _ = sink.0.send(crate::host::WorldEvent::OutputSubmitted {
            run_id: md.run_id.clone(),
            agent_id: agent_id.to_string(),
            stage: output.stage.clone(),
            output: crate::host::capped_output(output),
        });
    }
}

/// Whether a call in `calls` other than a `submit_output` failed without
/// reaching the tool lane: refused, cut off, not offered, or blocked. `results`
/// are the calls the dispatcher answered itself.
pub(super) fn any_refused(
    calls: &[crate::components::ToolCall],
    results: &[(String, String)],
) -> bool {
    results.iter().any(|(id, text)| {
        call_had_no_effect(text)
            && calls
                .iter()
                .any(|c| c.tool_id == *id && !crate::output_tool::is_output_tool(&c.name))
    })
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
    /// [`repeats`](Self::repeats) as the batch arrived.
    start_repeats: u32,
    /// Whether a call in this batch handed back an answer: one recorded, or
    /// the one already held, again.
    answered: bool,
    /// What each refused `submit_output` call in this batch was told.
    refused: Vec<String>,
}

impl<'a> BatchSubmissions<'a> {
    /// The batch's starting point. A batch run again after a gate prompt
    /// (`resolved`) starts where it started the first time. Otherwise the
    /// answer the agent holds (`recorded`) is this visit's when `kept` says it
    /// was recorded in this visit, or, with nothing kept (a daemon restart
    /// loses it), when this stage recorded it since the visit began at
    /// `entered_at`.
    pub(super) fn new(
        stage: Option<&StageDef>,
        state: &AgentState,
        recorded: Option<&'a crate::persistence::FinalOutput>,
        kept: Option<&VisitSubmissions>,
        resolved: Option<&'a crate::gate_prompt::GateResolved>,
        entered_at: Option<i64>,
    ) -> Self {
        let recorded = recorded.map(|o| &o.0);
        let (held, repeats) = match (resolved, kept.filter(|k| k.visit == state.current_visit)) {
            (Some(r), _) => r
                .submissions
                .as_ref()
                .map_or((None, 0), |s| (s.answer.as_ref(), s.repeats)),
            (None, Some(k)) => (recorded, k.repeats),
            (None, None) => {
                let since = |o: &&FinalOutput| {
                    o.stage == state.current_stage
                        && entered_at.is_some_and(|t| o.submitted_at >= t)
                };
                (recorded.filter(since), 0)
            }
        };
        Self {
            held,
            guarded: stage.is_some_and(|s| s.mode.interactive().is_none()),
            submitted: None,
            produced: Vec::new(),
            resubmissions: 0,
            repeats,
            start_repeats: repeats,
            answered: false,
            refused: Vec::new(),
        }
    }

    /// Where the batch started, for its gate hold.
    pub(super) fn start(&self) -> HeldSubmission {
        HeldSubmission {
            answer: self.held.cloned(),
            repeats: self.start_repeats,
        }
    }

    /// Whether a `submit_output` call with `args` hands back the answer the
    /// visit held as the batch arrived, which [`apply`](Self::apply) answers
    /// as a repeat without judging it again.
    pub(super) fn repeats_held(
        &self,
        args: &serde_json::Value,
        workdir: Option<&std::path::Path>,
    ) -> bool {
        self.guarded
            && self
                .held
                .is_some_and(|held| crate::output_tool::repeats(args, held, workdir))
    }

    /// Note a call the batch finished before a restart, whose result was
    /// `text`: a `submit_output` that was accepted, or answered as a repeat,
    /// handed back the answer.
    pub(super) fn recovered(&mut self, name: &str, text: &str) {
        self.answered |= crate::output_tool::is_output_tool(name)
            && (text.starts_with(crate::output_tool::ACCEPTED) || text == REPEATED_SUBMISSION);
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
        } else {
            self.refused.push(text.clone());
        }
        text
    }

    /// Keep the batch's refusals against the stage's visit, or forget the
    /// stage's once it has an answer. `prior` is what the agent held.
    pub(super) fn note_refusals(
        &self,
        agent: &mut EntityCommands,
        state: &AgentState,
        prior: Option<&SubmissionRefusals>,
    ) {
        let mut next = prior.cloned().unwrap_or_default();
        if self.submitted.is_some() {
            next.0.remove(&state.current_stage);
        } else if let Some(last) = self.refused.last() {
            let before = next
                .0
                .get(&state.current_stage)
                .filter(|r| r.visit == state.current_visit)
                .map_or(0, |r| r.count);
            next.0.insert(
                state.current_stage.clone(),
                StageRefusals {
                    visit: state.current_visit.clone(),
                    count: before + self.refused.len() as u32,
                    last: super::one_line(last, REFUSAL_CHARS),
                },
            );
        } else {
            return;
        }
        agent.insert(next);
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
