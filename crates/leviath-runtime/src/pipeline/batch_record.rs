//! A tool batch as the run's journal is told about it.

use crate::runfile::record::RunRecord;

/// One dispatched batch, in the shape the journal records it.
///
/// Gathered once and borrowed, because two paths write the same records and
/// neither can see the other's: a batch with lane work journals them with an
/// ack the exec waits on, and a batch the dispatcher resolved entirely
/// journals them on its way out. Two copies of the field list would be two
/// chances for one of them to stop carrying something.
pub(crate) struct BatchDispatch<'a> {
    /// The calls, in the order the model asked for them.
    pub(crate) calls: &'a [crate::components::ToolCall],
    /// The execution id minted for each, by provider call id.
    pub(crate) executions: &'a std::collections::HashMap<String, String>,
    /// The results the dispatcher already has, by provider call id. A call with
    /// one here never reaches the lane and no completion record will follow it.
    pub(crate) inline: &'a [(String, String)],
    /// Results carried from before a restart.
    pub(crate) recovered: &'a [crate::tool_bridge::ToolResult],
    /// Whether the run's file already records the batch as dispatched: it was
    /// brought back from the file, and keeps the executions it was sent as.
    pub(crate) resumed: bool,
    /// The stage the batch was dispatched in.
    pub(crate) stage_index: usize,
    /// The stage-local iteration that produced it.
    pub(crate) iteration: usize,
    /// The stay in the stage it was dispatched during.
    pub(crate) visit_id: &'a str,
    /// The provider attempt whose answer asked for the calls.
    pub(crate) requested_by: &'a str,
    /// The assistant text of the turn that issued them.
    pub(crate) response: &'a str,
}

impl BatchDispatch<'_> {
    /// What the journal is told, with `lane` the calls going to the tool lane.
    ///
    /// A batch dispatched the first time is one batch record. One the run's
    /// file already records is not recorded again: only the results settled
    /// now are, each a completion of the execution it was sent as (a call a
    /// stop interrupted ends unobserved), and then the calls sent to the lane
    /// again, under the executions they already had. A result the file
    /// already holds is not recorded twice.
    pub(crate) fn records(&self, lane: &[leviath_providers::ToolCall]) -> Vec<RunRecord> {
        if !self.resumed {
            return vec![self.record()];
        }
        let interrupted = self
            .recovered
            .iter()
            .filter(|(_, r)| {
                r.as_str()
                    .starts_with(crate::restore::INTERRUPTED_TOOL_RESULT)
            })
            .map(|(id, r)| (id, r.clone()));
        let settled = self
            .inline
            .iter()
            .map(|(id, text)| (id, leviath_core::region::EntryContent::from(text.clone())))
            .chain(interrupted);
        let mut records: Vec<RunRecord> = settled
            .map(|(id, result)| RunRecord::ToolCallDone {
                iteration: self.iteration,
                call_id: id.clone(),
                execution_id: self.execution_of(id),
                outcome: Some(crate::runfile::outcome_of(result.as_str())),
                result,
                at: chrono::Utc::now().timestamp(),
            })
            .collect();
        if !lane.is_empty() {
            records.push(RunRecord::ToolCallsResent {
                calls: lane
                    .iter()
                    .map(|c| (c.id.clone(), self.execution_of(&c.id)))
                    .collect(),
                requested_by: self.requested_by.to_string(),
                at: chrono::Utc::now().timestamp(),
            });
        }
        records
    }

    /// The execution the call `call_id` was dispatched as.
    fn execution_of(&self, call_id: &str) -> String {
        self.executions.get(call_id).cloned().unwrap_or_default()
    }

    /// The batch record of a batch dispatched the first time.
    fn record(&self) -> RunRecord {
        RunRecord::ToolBatch {
            calls: self
                .calls
                .iter()
                .map(|c| crate::runfile::record::ToolCallRecord {
                    id: c.tool_id.clone(),
                    execution_id: self.execution_of(&c.tool_id),
                    name: c.name.clone(),
                    arguments: c.arguments.to_string(),
                    result: self
                        .inline
                        .iter()
                        .find(|(id, _)| id == &c.tool_id)
                        .map(|(_, r)| r.clone().into())
                        .or_else(|| {
                            self.recovered
                                .iter()
                                .find(|(id, _)| id == &c.tool_id)
                                .map(|(_, r)| r.clone())
                        }),
                    thought_signature: c.thought_signature.clone(),
                })
                .collect(),
            at: chrono::Utc::now().timestamp(),
            stage_index: self.stage_index,
            iteration: self.iteration,
            visit_id: self.visit_id.to_string(),
            requested_by: self.requested_by.to_string(),
            response: self.response.to_string(),
        }
    }
}

/// Journal the files each execution produced, one record per execution.
///
/// Fire and forget, like the change records: nothing waits on it, and a run with
/// no lane writes nothing. Called from the same place the batch record is written
/// so the artifacts cannot land before the dispatch that made them.
pub(crate) fn journal_artifacts(
    journal: &super::JournalSender,
    run_id: &str,
    produced: &[(String, Vec<leviath_core::output::Artifact>)],
) {
    for (execution_id, artifacts) in produced {
        journal.record(
            run_id,
            RunRecord::ArtifactsProduced {
                execution_id: execution_id.clone(),
                artifacts: artifacts.clone(),
                at: chrono::Utc::now().timestamp(),
            },
        );
    }
}

#[cfg(test)]
#[path = "batch_record_tests.rs"]
mod tests;
