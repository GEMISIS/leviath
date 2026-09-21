//! Tests for the frame vocabulary: that every frame the daemon can send has a
//! type here, and that each one's fields survive the conversion.

use super::{InteractionKind, RunEvent, RunEventType};
use crate::commands::serve::events::ServerEvent;

/// One of every frame the daemon can send.
///
/// The list is the point: a frame added to the daemon without a type here is
/// one a subscriber cannot see, and the test below fails until it has one.
fn every_frame() -> Vec<ServerEvent> {
    vec![
        ServerEvent::AgentSpawned {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            parent_id: Some("root".to_string()),
            blueprint: "coder".to_string(),
        },
        ServerEvent::AgentStatus {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            status: "running".to_string(),
            stage: "build".to_string(),
            iteration: 3,
            tool_calls: 4,
            accepts_messages: true,
            wait_reason: None,
            title: Some("A title".to_string()),
        },
        ServerEvent::RunRenamed {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            title: "Named".to_string(),
        },
        ServerEvent::Tokens {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            prompt_tokens: 100,
            completion_tokens: 20,
            cached_tokens: 80,
            cache_write_tokens: 5,
        },
        ServerEvent::ContextUpdate {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            total_tokens: 1_000,
            max_tokens: 8_000,
        },
        ServerEvent::StageTransition {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            from: "plan".to_string(),
            to: "build".to_string(),
            iteration: 2,
        },
        ServerEvent::ToolCallStarted {
            execution_id: "x1".to_string(),
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            call_id: "call-1".to_string(),
            tool: "read_file".to_string(),
        },
        ServerEvent::ToolCallFinished {
            execution_id: "x1".to_string(),
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            call_id: "call-1".to_string(),
            tool: "read_file".to_string(),
            ok: false,
            summary: "[error] no such file".to_string(),
        },
        ServerEvent::Log {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            line: "a line".to_string(),
        },
        ServerEvent::AgentSpend {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            threshold_usd: 1.0,
            total_usd: 1.25,
            complete: false,
            stage: "build".to_string(),
        },
        ServerEvent::InteractionNeeded {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            request: serde_json::json!({
                "id": "ask-1",
                "kind": "multiple_choice",
                "prompt": "Which one?",
                "options": ["a", "b"],
                "stage_name": "plan",
                "required": true,
                "body": "a longer document",
            }),
        },
        ServerEvent::AgentCompleted {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            status: "complete".to_string(),
            result: None,
            final_output: Some(crate::commands::serve::types::FinalOutputResp {
                content: "the answer".to_string(),
                format: Some("markdown".to_string()),
                stage: "output".to_string(),
                submitted_at: 1_788_000_000,
                truncated: true,
                artifacts: Vec::new(),
            }),
        },
        ServerEvent::DaemonLink {
            connected: true,
            daemon: Some(
                leviath_runtime::control_socket::DaemonIdentity::this_process("test-build"),
            ),
            restarted: true,
            restart_advised: Some("restart lev serve".to_string()),
        },
        ServerEvent::ConfigHealth {
            healthy: true,
            path: "/config.toml".to_string(),
            error: None,
            config_mtime: Some(1_788_000_000),
        },
        ServerEvent::UpdateProgress {
            job_id: "job-1".to_string(),
            step: "binary".to_string(),
            status: "running".to_string(),
            detail: "downloading".to_string(),
        },
        ServerEvent::UpdateFinished {
            job_id: "job-1".to_string(),
            status: "complete".to_string(),
            restart_required: true,
            job: serde_json::json!({}),
        },
    ]
}

/// Every frame has a type, and every type is reachable from a frame.
///
/// `EVENTS_DROPPED` is the one type no frame produces: it is this server's own
/// signal that a subscriber fell behind.
#[test]
fn every_frame_has_its_own_type() {
    let mut seen: Vec<RunEventType> = every_frame().iter().map(RunEventType::of).collect();
    let before = seen.len();
    seen.sort_by_key(|kind| format!("{kind:?}"));
    seen.dedup();
    assert_eq!(seen.len(), before, "no two frames share a type");
    assert_eq!(before, 16, "one type per frame the daemon sends");
}

/// Every frame converts, and the fields a client reads survive.
#[test]
fn every_frame_converts_with_its_fields() {
    for frame in every_frame() {
        let kind = RunEventType::of(&frame);
        let event = RunEvent::from(frame);
        match (kind, event) {
            (RunEventType::RunSpawned, RunEvent::RunSpawned(spawned)) => {
                assert_eq!(spawned.run_id, "run-a");
                assert_eq!(spawned.parent_id.as_deref(), Some("root"));
                assert_eq!(spawned.blueprint, "coder");
            }
            (RunEventType::RunStatusChanged, RunEvent::RunStatusChanged(status)) => {
                assert_eq!(status.status, "running");
                assert_eq!(status.iteration, 3);
                assert_eq!(status.tool_calls, 4);
                assert!(status.accepts_messages);
                assert_eq!(status.title.as_deref(), Some("A title"));
            }
            (RunEventType::RunRenamed, RunEvent::RunRenamed(renamed)) => {
                assert_eq!(renamed.title, "Named");
            }
            (RunEventType::Tokens, RunEvent::TokensUpdated(tokens)) => {
                assert_eq!(tokens.prompt_tokens.0, 100);
                assert_eq!(tokens.cached_tokens.0, 80);
            }
            (RunEventType::ContextUpdate, RunEvent::ContextUpdated(context)) => {
                assert_eq!(context.total_tokens, 1_000);
                assert_eq!(context.max_tokens, 8_000);
            }
            (RunEventType::StageTransition, RunEvent::StageTransition(moved)) => {
                assert_eq!(moved.from, "plan");
                assert_eq!(moved.to, "build");
                assert_eq!(moved.iteration, 2);
            }
            (RunEventType::ToolCallStarted, RunEvent::ToolCallStarted(started)) => {
                assert_eq!(started.call_id, "call-1");
                assert_eq!(started.tool, "read_file");
            }
            (RunEventType::ToolCallFinished, RunEvent::ToolCallFinished(finished)) => {
                // A finish is not a success on its own, which is why `ok` is
                // part of the frame.
                assert!(!finished.ok);
                assert_eq!(finished.summary, "[error] no such file");
            }
            (RunEventType::Log, RunEvent::LogLine(line)) => {
                assert_eq!(line.line, "a line");
            }
            (RunEventType::RunSpend, RunEvent::RunSpend(spend)) => {
                assert_eq!(spend.threshold_usd.0, 1.0);
                assert_eq!(spend.total_usd.0, 1.25);
                // Incomplete, so a client must not show it as a final figure.
                assert!(!spend.complete);
            }
            (RunEventType::InteractionNeeded, RunEvent::InteractionNeeded(needed)) => {
                let request = needed.request.expect("the request reads");
                assert_eq!(request.id, "ask-1");
                assert_eq!(request.kind, InteractionKind::MultipleChoice);
                assert_eq!(request.prompt, "Which one?");
                assert_eq!(request.options, vec!["a", "b"]);
                assert_eq!(request.body.as_deref(), Some("a longer document"));
                assert_eq!(request.stage_name, "plan");
                assert!(request.required);
            }
            (RunEventType::RunCompleted, RunEvent::RunCompleted(completed)) => {
                assert_eq!(completed.status, "complete");
                assert!(completed.error.is_none());
                let output = completed.final_output.expect("an answer");
                assert_eq!(output.content, "the answer");
                assert_eq!(output.format.as_deref(), Some("markdown"));
                assert_eq!(output.submitted_at.0, 1_788_000_000);
                assert!(output.truncated);
            }
            (RunEventType::DaemonLink, RunEvent::DaemonLinkChanged(link)) => {
                assert!(link.connected);
                assert!(link.restarted);
                assert_eq!(link.restart_advised.as_deref(), Some("restart lev serve"));
                let daemon = link.daemon.expect("it said who it is");
                assert_eq!(daemon.build, "test-build");
                assert!(daemon.pid > 0);
            }
            (RunEventType::ConfigHealth, RunEvent::ConfigHealthChanged(health)) => {
                assert!(health.healthy);
                assert_eq!(health.path, "/config.toml");
                assert!(health.error.is_none());
                assert_eq!(health.config_mtime.map(|t| t.0), Some(1_788_000_000));
            }
            (RunEventType::UpdateProgress, RunEvent::UpdateProgress(progress)) => {
                assert_eq!(progress.step, "binary");
                assert_eq!(progress.detail, "downloading");
            }
            (RunEventType::UpdateFinished, RunEvent::UpdateFinished(finished)) => {
                assert_eq!(finished.status, "complete");
                assert!(finished.restart_required);
            }
            (kind, _) => panic!("{kind:?} converted to another type"),
        }
    }
}

/// A request shape this build cannot read reads as absent, rather than failing
/// the subscription.
///
/// The frame forwards the daemon's own JSON, so a newer daemon's new
/// interaction kind arrives as something this build does not know. One
/// unreadable request must not end a subscription that is also carrying every
/// other frame.
#[test]
fn an_unreadable_request_reads_as_absent() {
    let frame = ServerEvent::InteractionNeeded {
        agent_id: "a".to_string(),
        run_id: "run-a".to_string(),
        request: serde_json::json!({ "kind": "something_new" }),
    };
    match RunEvent::from(frame) {
        RunEvent::InteractionNeeded(needed) => {
            assert!(needed.request.is_none());
            assert_eq!(needed.run_id, "run-a");
        }
        _ => panic!("an interaction frame converts to an interaction frame"),
    }
}

/// An error on the config-health frame reaches the client as one line.
#[test]
fn a_config_failure_carries_its_message() {
    let frame = ServerEvent::ConfigHealth {
        healthy: false,
        path: "/config.toml".to_string(),
        error: Some(crate::commands::serve::config_types::ConfigErrorInfo {
            kind: "parse".to_string(),
            path: "/config.toml".to_string(),
            message: "expected a table".to_string(),
            line: Some(3),
            column: Some(1),
            key: None,
            since: 1_788_000_000,
            note: "the file did not parse".to_string(),
        }),
        config_mtime: None,
    };
    match RunEvent::from(frame) {
        RunEvent::ConfigHealthChanged(health) => {
            assert!(!health.healthy);
            assert_eq!(health.error.as_deref(), Some("expected a table"));
            assert!(health.config_mtime.is_none());
        }
        _ => panic!("a config frame converts to a config frame"),
    }
}

/// Every interaction kind the daemon has maps to one value here.
#[test]
fn every_interaction_kind_maps_to_one_value() {
    use leviath_core::interaction::InteractionKind as Core;
    let cases = [
        (Core::FreeText, InteractionKind::FreeText),
        (Core::MultipleChoice, InteractionKind::MultipleChoice),
        (Core::Confirm, InteractionKind::Confirm),
        (Core::ToolApproval, InteractionKind::ToolApproval),
        (Core::EditText, InteractionKind::EditText),
    ];
    for (core, expected) in cases {
        assert_eq!(InteractionKind::from(&core), expected, "{core:?}");
    }
}
