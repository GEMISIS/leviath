//! The questions asked by runs the daemon holds off this machine.
//!
//! A run whose provider was taken away, say, is not placed in the world, so
//! the hub has nothing open for it and its question is in no listing the
//! daemon keeps. The question is still on the run's file, and the daemon
//! lists the run as held with what to put back. Nothing can answer such a
//! question while the run is held: once the run is placed again, its
//! question is asked anew under a new id.
//!
//! Every surface reads them here and says the same thing about them: the CLI
//! lists them marked held, GraphQL as `heldInteractions`, and an answer or a
//! read that names one is refused as [`ServeError::Held`] rather than told
//! there is no such question.

use leviath_core::run_meta::WaitReason;
use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};
use leviath_runtime::state::OpenInteraction;

use super::error::ServeError;

/// A question a run asked that the daemon holds off this machine.
#[derive(Debug, Clone)]
pub(crate) struct HeldQuestion {
    /// The run that asked it.
    pub(crate) run_id: String,
    /// The stage the run is in.
    pub(crate) stage: String,
    /// What it asked, as the run's file keeps it.
    pub(crate) question: OpenInteraction,
    /// What to put back so the machine can take the run again.
    pub(crate) remedy: String,
}

impl HeldQuestion {
    /// Why nothing can answer this question yet, and what does.
    pub(crate) fn refusal(&self) -> String {
        format!(
            "'{}' was asked by run '{}', which this machine cannot take back as it stands, so \
             nothing can answer it yet: {}. Once the run is back, the question reopens under a \
             new id",
            self.question.id, self.run_id, self.remedy
        )
    }
}

/// The questions open on the runs the daemon holds off this machine, read off
/// each one's file. Empty when the daemon does not list its runs.
pub(crate) async fn held_questions(control: &ControlClient) -> Vec<HeldQuestion> {
    let runs = match control.request(&ControlRequest::List).await {
        Ok(ControlResponse::List { runs, .. }) => runs,
        _ => Vec::new(),
    };
    let held: Vec<(String, String, String)> = runs
        .into_iter()
        .filter_map(|row| match row.wait_reason {
            Some(WaitReason::NeedsSetup { remedy, .. }) => Some((row.run_id, row.stage, remedy)),
            _ => None,
        })
        .collect();
    super::super::blocking::blocking(move || {
        held.into_iter()
            .flat_map(|(run_id, stage, remedy)| {
                let open = crate::runstate::run_file::tail_in(&crate::runstate::run_dir(&run_id))
                    .map(|tail| tail.state.interactions)
                    .unwrap_or_default();
                open.into_iter().map(move |question| HeldQuestion {
                    run_id: run_id.clone(),
                    stage: stage.clone(),
                    question,
                    remedy: remedy.clone(),
                })
            })
            .collect()
    })
    .await
}

/// `otherwise`, or the refusal for the held question `named` picks.
///
/// Asked only once nothing open answered to the name, so the daemon is listed
/// only on a miss.
pub(crate) async fn or_held(
    control: &ControlClient,
    named: impl Fn(&HeldQuestion) -> bool,
    otherwise: ServeError,
) -> ServeError {
    match held_questions(control).await.iter().find(|h| named(h)) {
        Some(held) => ServeError::Held(held.refusal()),
        None => otherwise,
    }
}

/// A held run on disk with `question` open on its file, and the row the
/// daemon lists it with. For the tests of every surface that reads one.
#[cfg(test)]
pub(crate) fn seed_held(run_id: &str, question: &str) -> leviath_runtime::host::RunListEntry {
    crate::runstate::create_run(&crate::runstate::RunMeta {
        status: crate::runstate::RunStatus::Paused,
        ..crate::test_support::fixtures::run_meta(run_id)
    })
    .unwrap();
    let path = crate::runstate::run_file::path_in(&crate::runstate::run_dir(run_id));
    let mut writer = leviath_runtime::runfile::RunFileWriter::open(
        &path,
        leviath_runtime::runfile::CheckpointPolicy::default(),
    )
    .unwrap();
    let mut next = writer.state().clone();
    next.interactions.push(OpenInteraction {
        id: question.to_string(),
        prompt: "What colour?".to_string(),
        options: vec!["red".to_string()],
    });
    let at = writer.state().seq as i64 + 1;
    writer.record(next, at, Vec::new()).unwrap();
    leviath_runtime::host::RunListEntry {
        run_id: run_id.to_string(),
        title: None,
        status: leviath_runtime::components::AgentStatus::Paused,
        wait_reason: Some(WaitReason::NeedsSetup {
            blocker: leviath_core::run_meta::SetupBlocker::ProviderMissing,
            remedy: "configure 'openai' again, then `lev resume` this run".to_string(),
        }),
        stage: "ask".to_string(),
        stage_index: None,
        num_stages: None,
        iteration: 0,
        tool_calls: 0,
        last_progress_at: None,
        started_at: None,
        active: None,
        unattended: false,
        yolo_profile: None,
        empty_output: false,
        splits_degraded: 0,
        broken_scripts: Vec::new(),
        read_paths: None,
        has_final_output: false,
        may_never_finish: Vec::new(),
    }
}

/// The daemon's `List` reply naming `rows`.
#[cfg(test)]
pub(crate) fn listing(rows: Vec<leviath_runtime::host::RunListEntry>) -> ControlResponse {
    ControlResponse::List {
        runs: rows,
        finished: Vec::new(),
        health: Default::default(),
    }
}

#[cfg(test)]
#[path = "held_tests.rs"]
mod tests;
