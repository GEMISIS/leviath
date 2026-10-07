//! Reading a run's questions back out of its run file.

use leviath_core::interaction::{ApprovalScope, InteractionKind, Settlement};
use leviath_runtime::state::{OpenInteraction, RunEvent};

use super::super::run_file::tests::{garbage, recorded, step};
use super::read;

/// A question put to a person.
fn question(id: &str, prompt: &str, options: &[&str]) -> OpenInteraction {
    OpenInteraction {
        id: id.to_string(),
        prompt: prompt.to_string(),
        options: options.iter().map(|o| o.to_string()).collect(),
    }
}

/// The answer event a settlement is recorded as.
fn answered(id: &str, settlement: &Settlement) -> RunEvent {
    RunEvent::Answered {
        id: id.to_string(),
        answer: serde_json::to_string(settlement).unwrap(),
    }
}

#[tokio::test]
async fn each_question_is_paired_with_how_it_settled() {
    crate::runstate::with_isolated_runs_dir_async("interactions-read", |_d| async move {
        let run_id = recorded();
        let approval = Settlement::Answered {
            approved: Some(true),
            scope: Some(ApprovalScope::Once),
            choice: None,
            text: None,
            feedback: None,
        };
        let picked = Settlement::Answered {
            approved: None,
            scope: None,
            choice: Some(1),
            text: None,
            feedback: None,
        };
        // Asked at 10, in the stage the run is in.
        step(&run_id, 10, Vec::new(), |s| {
            s.interactions = vec![
                question("q-approve", "Run the tests?", &[]),
                question("q-pick", "Which one?", &["a", "b"]),
            ];
        });
        // Asked again in the next step: still the same question.
        step(&run_id, 15, vec![RunEvent::Log("asked".into())], |s| {
            s.cursor.iteration += 1;
        });
        step(
            &run_id,
            20,
            vec![
                answered("q-approve", &approval),
                answered("q-pick", &picked),
            ],
            |s| s.interactions.clear(),
        );
        // A question settled with words that are not a recorded settlement,
        // and one the file never saw asked.
        step(
            &run_id,
            30,
            vec![
                RunEvent::Answered {
                    id: "q-typed".into(),
                    answer: "just do it".into(),
                },
                answered("q-timeout", &Settlement::TimedOut),
            ],
            |s| s.cursor.iteration += 1,
        );

        let records = read(&run_id).unwrap();
        let ids: Vec<&str> = records.iter().map(|r| r.request_id.as_str()).collect();
        assert_eq!(ids, vec!["q-approve", "q-pick", "q-typed", "q-timeout"]);

        let approve = &records[0];
        assert_eq!(approve.kind, InteractionKind::ToolApproval);
        assert_eq!(approve.prompt, "Run the tests?");
        assert_eq!(approve.asked_at, 10);
        assert_eq!(approve.at, 20);
        assert_eq!(approve.settlement, approval);
        assert!(!approve.stage.is_empty());

        assert_eq!(records[1].kind, InteractionKind::MultipleChoice);
        assert_eq!(records[1].settlement, picked);

        let typed = &records[2];
        assert_eq!(typed.kind, InteractionKind::FreeText);
        assert_eq!(typed.prompt, "");
        assert_eq!(typed.asked_at, 30);
        assert_eq!(
            typed.settlement,
            Settlement::Answered {
                approved: None,
                scope: None,
                choice: None,
                text: Some("just do it".into()),
                feedback: None,
            }
        );
        assert_eq!(records[3].settlement, Settlement::TimedOut);
        assert_eq!(records[3].kind, InteractionKind::FreeText);
    })
    .await;
}

#[tokio::test]
async fn a_run_with_no_file_asked_nothing_and_an_unreadable_one_is_an_error() {
    crate::runstate::with_isolated_runs_dir_async("interactions-none", |_d| async move {
        assert!(read("ghost").unwrap().is_empty());
        garbage("broken", b"not a run file");
        let stepped = recorded();
        super::super::run_file::tests::bad_step(&stepped, 1);
        assert_eq!(read(&stepped).unwrap_err().code(), "INTERNAL");
        assert_eq!(read("broken").unwrap_err().code(), "INTERNAL");
    })
    .await;
}

/// A question kept whole reads back with its own kind, tool and stage, and a
/// settlement that is not one reads as the text it was.
#[tokio::test]
async fn a_question_kept_whole_reads_back_whole() {
    use leviath_runtime::state::journal::{QuestionKind, SettledState};
    crate::runstate::with_isolated_runs_dir_async("interactions-whole", |_d| async move {
        let run_id = recorded();
        let settled = |id: &str, kind: QuestionKind, settlement: &str| {
            RunEvent::Settled(Box::new(SettledState {
                id: id.to_string(),
                kind,
                tool: Some("shell".into()),
                prompt: "?".into(),
                stage: "plan".into(),
                settlement: settlement.to_string(),
                asked_at: 3,
            }))
        };
        let timed_out = serde_json::to_string(&Settlement::TimedOut).unwrap();
        step(
            &run_id,
            10,
            vec![
                settled("q1", QuestionKind::FreeText, "typed words"),
                settled("q2", QuestionKind::MultipleChoice, &timed_out),
                settled("q3", QuestionKind::Confirm, &timed_out),
                settled("q4", QuestionKind::ToolApproval, &timed_out),
                settled("q5", QuestionKind::EditText, &timed_out),
            ],
            |s| s.cursor.iteration += 1,
        );
        let asked = read(&run_id).unwrap();
        let kinds: Vec<InteractionKind> = asked.iter().map(|q| q.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                InteractionKind::FreeText,
                InteractionKind::MultipleChoice,
                InteractionKind::Confirm,
                InteractionKind::ToolApproval,
                InteractionKind::EditText
            ]
        );
        assert_eq!(asked[0].stage, "plan");
        assert_eq!(asked[0].tool.as_deref(), Some("shell"));
        assert_eq!(asked[0].asked_at, 3);
        assert_eq!(asked[0].at, 10);
        assert!(matches!(
            &asked[0].settlement,
            Settlement::Answered { text: Some(t), .. } if t == "typed words"
        ));
        assert_eq!(asked[1].settlement, Settlement::TimedOut);
    })
    .await;
}
