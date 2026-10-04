//! The selected run's answer, read again only when it may have changed.
//!
//! The detail view asks for the answer on every frame: it decides whether
//! the `[f] final` chip is offered, and the Final and Output views show it.
//! Reading it means reading the run's record off its run file and then the
//! answer, so the answer is kept with the stamps of the two files it comes
//! from, and a frame that finds both unchanged costs two stats.

use std::cell::RefCell;
use std::sync::Arc;

use leviath_core::FinalOutput;

use super::state::Dashboard;
use crate::runstate::{self, FileStamp};

/// The last answer read, for the run it belongs to.
#[derive(Default)]
pub(super) struct AnswerCache(RefCell<Option<Answer>>);

struct Answer {
    run_id: String,
    /// The run file's stamp and the answer sidecar's, when it was read.
    stamps: (Option<FileStamp>, Option<FileStamp>),
    answer: Option<Arc<FinalOutput>>,
}

impl Dashboard {
    /// The answer `run_id` handed back, as [`runstate::read_final_output`]
    /// reads it, or `None` while it has none.
    pub(super) fn final_output_of(&self, run_id: &str) -> Option<Arc<FinalOutput>> {
        let stamps = runstate::answer_stamps(run_id);
        let mut cached = self.answers.0.borrow_mut();
        if let Some(known) = cached
            .as_ref()
            .filter(|known| known.run_id == run_id && known.stamps == stamps)
        {
            return known.answer.clone();
        }
        let answer = runstate::read_final_output(run_id).map(Arc::new);
        *cached = Some(Answer {
            run_id: run_id.to_string(),
            stamps,
            answer: answer.clone(),
        });
        answer
    }
}

#[cfg(test)]
mod tests {
    use crate::commands::dashboard::test_support::{
        make_test_dashboard, seed_run_with_final_output,
    };
    use crate::runstate;

    /// A frame that finds the run's files unchanged is answered from memory;
    /// a new answer, or another run, is read.
    #[test]
    fn the_answer_is_read_again_only_when_its_files_change() {
        runstate::with_isolated_runs_dir("dash-answer-cache", |_d| {
            let dash = make_test_dashboard();
            assert!(dash.final_output_of("run-a").is_none());
            seed_run_with_final_output("run-a", "main", "first");
            let first = dash.final_output_of("run-a").expect("an answer");
            assert_eq!(first.content, "first");
            let again = dash.final_output_of("run-a").expect("an answer");
            assert!(std::sync::Arc::ptr_eq(&first, &again), "read from memory");

            seed_run_with_final_output("run-b", "main", "other");
            assert_eq!(dash.final_output_of("run-b").unwrap().content, "other");

            // A rewritten answer is a changed file, and is read.
            runstate::write_final_output(&runstate::run_dir("run-a"), "second, longer").unwrap();
            assert_eq!(
                dash.final_output_of("run-a").unwrap().content,
                "second, longer"
            );
        });
    }
}
