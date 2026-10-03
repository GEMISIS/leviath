//! Reading a run's model calls back out of its run file.

use leviath_runtime::runfile::record::{AttemptOutcome, Retry};
use leviath_runtime::spec::names::ModelRef;
use leviath_runtime::state::RunEvent;

use super::super::run_file::tests::{garbage, recorded, step};
use super::{attempt, read};

fn model(text: &str) -> ModelRef {
    ModelRef::parse(text).unwrap()
}

/// A model call that answered.
fn called(id: &str, on: &str, finish: Option<&str>) -> RunEvent {
    RunEvent::Inference {
        attempt: id.to_string(),
        model: model(on),
        spend: Default::default(),
        finish_reason: finish.map(str::to_string),
        kind: Default::default(),
        stage: None,
        iteration: 0,
    }
}

/// A call a run file kept only as its bill is not listed: a run converted
/// from a journal that recorded no attempts lists none, as the release that
/// wrote it did. A move to another model with no call kept whole to follow
/// stands on its own, for the call it gave up on.
#[tokio::test]
async fn only_calls_kept_whole_are_listed_and_a_lone_move_stands_alone() {
    crate::runstate::with_isolated_runs_dir_async("inferences-read", |_d| async move {
        let run_id = recorded();
        step(
            &run_id,
            10,
            vec![called("a1", "anthropic/m", Some("tool_call"))],
            |s| s.cursor.iteration += 1,
        );
        step(
            &run_id,
            20,
            vec![
                RunEvent::Failover {
                    from: model("anthropic/m"),
                    to: model("openai/n"),
                    reason: "rate_limited".into(),
                },
                called("a2", "openai/n", None),
                RunEvent::Log("not a call".into()),
            ],
            |s| s.cursor.iteration += 1,
        );
        // A move whose models name no provider.
        step(
            &run_id,
            30,
            vec![RunEvent::Failover {
                from: model("bare"),
                to: model("other"),
                reason: "down".into(),
            }],
            |s| s.cursor.iteration += 1,
        );

        let calls = read(&run_id).unwrap();
        assert_eq!(calls.len(), 2, "only the moves are listed");
        let failed = &calls[0];
        assert_eq!(failed.record.id, "");
        assert_eq!(failed.record.model, "m");
        assert!(!failed.record.stage.is_empty());
        assert!(matches!(
            failed.record.outcome,
            AttemptOutcome::Failed {
                next: Retry::Reported,
                ..
            }
        ));
        let moved = failed.failover.as_ref().unwrap();
        assert_eq!(
            (moved.to_provider.as_str(), moved.to_model.as_str()),
            ("openai", "n")
        );
        assert_eq!(moved.reason, "rate_limited");
        assert_eq!(moved.iteration, 1);
        assert_eq!(failed.record.at, 20);
        assert_eq!(calls[1].record.provider, "");

        assert!(attempt(&run_id, "a1").unwrap().is_none());
        assert!(
            attempt(&run_id, "").unwrap().is_none(),
            "an empty id names nothing"
        );
    })
    .await;
}

#[tokio::test]
async fn a_run_with_no_file_made_no_calls_and_an_unreadable_one_is_an_error() {
    crate::runstate::with_isolated_runs_dir_async("inferences-none", |_d| async move {
        assert!(read("ghost").unwrap().is_empty());
        garbage("broken", b"not a run file");
        let stepped = recorded();
        super::super::run_file::tests::bad_step(&stepped, 1);
        assert_eq!(read(&stepped).unwrap_err().code(), "INTERNAL");
        assert_eq!(read("broken").unwrap_err().code(), "INTERNAL");
        assert_eq!(attempt("broken", "a1").unwrap_err().code(), "INTERNAL");
    })
    .await;
}

/// A call kept whole reads back with every capture state its request can be
/// in, and a move after a call that answered stands on its own.
#[tokio::test]
async fn a_call_kept_whole_reads_back_whole() {
    use leviath_core::JsonDoc;
    use leviath_runtime::runfile::record::CaptureStatus;
    use leviath_runtime::state::journal::{
        AttemptOutcomeState, AttemptState, CaptureState, ModelInputState, RequestDigestState,
    };
    crate::runstate::with_isolated_runs_dir_async("inferences-whole", |_d| async move {
        let run_id = recorded();
        let kept = |id: &str, capture: CaptureState| {
            RunEvent::Attempt(Box::new(AttemptState {
                id: id.to_string(),
                number: 1,
                provider: "anthropic".into(),
                model: "m".into(),
                outcome: AttemptOutcomeState::Succeeded,
                finish_reason: None,
                stopped_for: None,
                duration_ms: 1,
                backoff_ms: 0,
                digest: RequestDigestState {
                    system_hash: 1,
                    messages: 1,
                    tools: 0,
                    max_tokens: 1,
                    temperature: 0.0,
                },
                model_input: Some(ModelInputState {
                    capture,
                    request: None,
                    bytes: 0,
                    source_context_digest: String::new(),
                    parameters: [("t".to_string(), JsonDoc::new(serde_json::json!(1)))].into(),
                    tool_catalog_version: String::new(),
                    assembly_version: String::new(),
                }),
            }))
        };
        step(
            &run_id,
            10,
            vec![
                kept("a1", CaptureState::Retained),
                kept("a2", CaptureState::NotCaptured),
                kept("a3", CaptureState::Redacted),
                kept("a4", CaptureState::Expired),
                RunEvent::Failover {
                    from: model("anthropic/m"),
                    to: model("anthropic/n"),
                    reason: "after an answer".into(),
                },
            ],
            |s| s.cursor.iteration += 1,
        );
        let calls = read(&run_id).unwrap();
        let captures: Vec<CaptureStatus> = calls
            .iter()
            .filter_map(|c| c.record.model_input.as_ref().map(|m| m.capture_status))
            .collect();
        assert_eq!(
            captures,
            vec![
                CaptureStatus::Retained,
                CaptureStatus::NotCaptured,
                CaptureStatus::Redacted,
                CaptureStatus::Expired
            ]
        );
        assert_eq!(
            calls[0].record.model_input.as_ref().unwrap().parameters["t"],
            serde_json::json!(1)
        );
        assert_eq!(calls.len(), 5, "the move stands on its own");
        assert!(calls[3].failover.is_none());
        assert!(calls[4].failover.is_some());
    })
    .await;
}
