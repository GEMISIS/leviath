//! Tests for the frame vocabulary: that every frame the daemon can send has a
//! type here, that each one's fields survive the conversion, and that the
//! enums a subscription filters with cannot drift from the types they name.

use super::super::super::events::{ServerEvent, Stamped};
use super::super::super::update_job;
use super::{
    FRAME_VOCABULARY, MachineEventFrame, MachineEventType, RunEventFrame, RunEventType,
    UpdateJobEventFrame, machine_frame, run_frame, update_job_frame,
};
use crate::commands::serve::graphql::types::interaction::InteractionKind;
use crate::commands::serve::graphql::types::machine::ConfigErrorKind;
use crate::commands::serve::graphql::types::run::RunStatus;
use crate::commands::serve::graphql::types::update::{
    UpdateJobStatus, UpdateStep, UpdateStepStatus,
};

/// One finished update job, as the finish frame carries it.
fn job_record() -> update_job::UpdateJob {
    update_job::UpdateJob {
        id: "job-1".to_string(),
        status: update_job::JobStatus::Complete,
        steps: vec![update_job::UpdateStep {
            step: update_job::Step::Binary,
            status: update_job::StepStatus::Done,
            detail: "installed".to_string(),
        }],
        restart_required: true,
        restart_hint: Some("restart lev serve".to_string()),
        started_at: 1_788_000_000,
        finished_at: Some(1_788_000_060),
    }
}

/// One of every frame the daemon can send.
///
/// The list is the point: a frame added to the daemon without a type here is
/// one a subscriber cannot see, and the tests below fail until it has one.
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
            wait_reason: Some(leviath_core::run_meta::WaitReason::Children { outstanding: 2 }),
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
            step: update_job::Step::Binary,
            status: update_job::StepStatus::Running,
            detail: "downloading".to_string(),
        },
        ServerEvent::UpdateFinished {
            job_id: "job-1".to_string(),
            status: update_job::JobStatus::Complete,
            restart_required: true,
            job: job_record(),
        },
    ]
}

/// One frame, stamped as the bus would stamp it.
fn stamped(event: ServerEvent) -> Stamped {
    Stamped {
        seq: 7,
        at: 1_788_000_123,
        event,
    }
}

/// Every frame the daemon sends belongs to exactly one vocabulary.
///
/// A run frame has a `RunEventType` and no `MachineEventType`, and the other
/// way round. Neither answering, or both, is a frame a subscription could not
/// ask for or could ask for twice.
#[test]
fn every_frame_belongs_to_exactly_one_vocabulary() {
    let mut run = 0;
    let mut machine = 0;
    for frame in every_frame() {
        match (RunEventType::of(&frame), MachineEventType::of(&frame)) {
            (Some(_), None) => run += 1,
            (None, Some(_)) => machine += 1,
            (run, machine) => panic!("{run:?} and {machine:?} for one frame"),
        }
    }
    assert_eq!(run, 12, "one type per run frame");
    assert_eq!(machine, 4, "one type per machine frame");
}

/// Every type in the vocabulary a subscription filters with is the name of a
/// frame type in the schema, minus `Event`.
///
/// The enum, the frame types and the unions come out of one table, so this
/// cannot drift by an edit in one place. What it catches is a table row whose
/// two names were not written to match - `RunSpawned` beside `SpawnedEvent` -
/// which would leave a client filtering on a word the frames never use.
#[test]
fn every_event_type_matches_its_member() {
    let sdl = crate::commands::serve::graphql::sdl();
    for (type_name, value) in FRAME_VOCABULARY {
        assert_eq!(
            *type_name,
            format!("{value}Event"),
            "the type is the value plus `Event`"
        );
        assert!(
            sdl.contains(&format!("type {type_name} ")),
            "{type_name} is in the schema"
        );
        assert!(
            sdl.contains(&screaming(value)),
            "{} is a value in the schema",
            screaming(value)
        );
    }
}

/// One `PascalCase` name in `SCREAMING_SNAKE`, which is what async-graphql
/// writes an enum value as.
fn screaming(name: &str) -> String {
    let mut out = String::new();
    for (at, ch) in name.char_indices() {
        if ch.is_uppercase() && at > 0 {
            out.push('_');
        }
        out.extend(ch.to_uppercase());
    }
    out
}

/// Both interfaces reach the schema, and every frame that should implement one
/// says so.
///
/// Nothing returns either interface - a subscription yields a union - so they
/// are registered by name in `build_schema`. A registration that went missing
/// would take the interfaces and every `implements` clause with it, and no
/// query would fail until a client tried to use one.
#[test]
fn the_frame_interfaces_reach_the_schema() {
    let sdl = crate::commands::serve::graphql::sdl();
    assert!(sdl.contains("interface Event "), "the Event interface");
    assert!(
        sdl.contains("interface RunEvent "),
        "the RunEvent interface"
    );
    assert!(
        sdl.contains("type RunSpawnedEvent implements Event & RunEvent"),
        "a run frame implements both"
    );
    assert!(
        sdl.contains("type ConfigHealthChangedEvent implements Event"),
        "a machine frame implements Event"
    );
    // The two transport frames carry the stamp, so `... on Event { seq at }`
    // answers on them too. They are about no run, so they stay outside
    // `RunEvent`, and that is what separates what the daemon said from what
    // this server is saying about the subscription itself.
    assert!(
        sdl.contains("type SubscriptionOpenedEvent implements Event {"),
        "the opening frame is an Event"
    );
    assert!(
        sdl.contains("type EventsDroppedEvent implements Event {"),
        "the gap frame is an Event"
    );
    assert!(
        !sdl.contains("SubscriptionOpenedEvent implements Event & RunEvent"),
        "the opening frame is about no run"
    );
    assert!(
        !sdl.contains("EventsDroppedEvent implements Event & RunEvent"),
        "the gap frame is about no run"
    );
}

/// Every run frame converts, and the fields a client reads survive.
#[test]
fn every_run_frame_converts_with_its_fields() {
    for frame in every_frame() {
        // A machine frame is covered by the machine test below, and the link
        // frame - the one that answers on both sides - by its own test.
        let Some(kind) = RunEventType::of(&frame) else {
            continue;
        };
        let converted = run_frame(stamped(frame)).expect("a run frame converts");
        match (kind, converted) {
            (RunEventType::RunSpawned, RunEventFrame::RunSpawned(spawned)) => {
                assert_eq!(spawned.seq.0, 7);
                assert_eq!(spawned.at.0, 1_788_000_123);
                assert_eq!(spawned.run_id.as_str(), "run-a");
                assert_eq!(spawned.agent_id.as_str(), "a");
                assert_eq!(
                    spawned.parent_id.as_ref().map(|id| id.as_str()),
                    Some("root")
                );
                assert_eq!(spawned.blueprint_name, "coder");
            }
            (RunEventType::RunStatusChanged, RunEventFrame::RunStatusChanged(status)) => {
                assert_eq!(status.status, RunStatus::Running);
                assert_eq!(status.iteration.0, 3);
                assert_eq!(status.tool_calls.0, 4);
                assert!(status.accepts_messages);
                assert_eq!(status.title.as_deref(), Some("A title"));
                // Carried rather than dropped: a parked run a client cannot
                // see the reason for is a row somebody has to go and ask about.
                let waiting = status.wait_reason.as_ref().expect("the reason is carried");
                assert_eq!(waiting.outstanding, Some(2));
            }
            (RunEventType::RunRenamed, RunEventFrame::RunRenamed(renamed)) => {
                assert_eq!(renamed.title, "Named");
            }
            (RunEventType::TokensUpdated, RunEventFrame::TokensUpdated(tokens)) => {
                assert_eq!(tokens.prompt_tokens.0, 100);
                assert_eq!(tokens.completion_tokens.0, 20);
                assert_eq!(tokens.cached_tokens.0, 80);
                assert_eq!(tokens.cache_write_tokens.0, 5);
            }
            (RunEventType::ContextUpdated, RunEventFrame::ContextUpdated(context)) => {
                assert_eq!(context.total_tokens.0, 1_000);
                assert_eq!(context.max_tokens.0, 8_000);
            }
            (RunEventType::StageTransitioned, RunEventFrame::StageTransitioned(moved)) => {
                assert_eq!(moved.from, "plan");
                assert_eq!(moved.to, "build");
                assert_eq!(moved.iteration.0, 2);
            }
            (RunEventType::ToolCallStarted, RunEventFrame::ToolCallStarted(started)) => {
                assert_eq!(started.call_id, "call-1");
                assert_eq!(started.execution_id.as_str(), "x1");
                assert_eq!(started.tool, "read_file");
            }
            (RunEventType::ToolCallFinished, RunEventFrame::ToolCallFinished(finished)) => {
                // A finish is not a success on its own, which is why `ok` is
                // part of the frame.
                assert!(!finished.ok);
                assert_eq!(finished.call_id, "call-1");
                assert_eq!(finished.execution_id.as_str(), "x1");
                assert_eq!(finished.tool, "read_file");
                assert_eq!(finished.summary, "[error] no such file");
            }
            (RunEventType::LogLineWritten, RunEventFrame::LogLineWritten(line)) => {
                assert_eq!(line.line, "a line");
            }
            (RunEventType::SpendThresholdCrossed, RunEventFrame::SpendThresholdCrossed(spend)) => {
                assert_eq!(spend.threshold_usd.0, 1.0);
                assert_eq!(spend.total_usd.0, 1.25);
                assert_eq!(spend.stage, "build");
                // Incomplete, so a client must not show it as a final figure.
                assert!(!spend.complete);
            }
            (RunEventType::InteractionOpened, RunEventFrame::InteractionOpened(opened)) => {
                let request = opened.interaction.expect("the ask reads");
                assert_eq!(request.id, "ask-1");
                assert_eq!(request.kind, InteractionKind::MultipleChoice);
                assert_eq!(request.prompt, "Which one?");
                assert_eq!(request.options, vec!["a", "b"]);
                assert_eq!(request.body.as_deref(), Some("a longer document"));
                assert_eq!(request.stage_name, "plan");
                assert!(request.is_required);
            }
            (RunEventType::RunCompleted, RunEventFrame::RunCompleted(completed)) => {
                assert_eq!(completed.status, RunStatus::Complete);
                assert!(completed.error.is_none());
                let output = completed.final_output.expect("an answer");
                assert_eq!(output.content, "the answer");
                assert_eq!(output.format.as_deref(), Some("markdown"));
                assert_eq!(output.stage, "output");
                assert_eq!(output.submitted_at.0, 1_788_000_000);
                assert!(output.truncated);
            }
            (kind, _) => panic!("{kind:?} converted to another type"),
        }
    }
}

/// Every machine frame converts, and the fields a client reads survive.
#[test]
fn every_machine_frame_converts_with_its_fields() {
    for frame in every_frame() {
        let Some(kind) = MachineEventType::of(&frame) else {
            assert!(
                machine_frame(stamped(frame)).is_none(),
                "a run frame is not a machine frame"
            );
            continue;
        };
        let converted = machine_frame(stamped(frame)).expect("a machine frame converts");
        match (kind, converted) {
            (MachineEventType::DaemonLinkChanged, MachineEventFrame::DaemonLinkChanged(link)) => {
                assert_eq!(link.seq.0, 7);
                assert!(link.connected);
                assert!(link.restarted);
                assert_eq!(link.restart_advised.as_deref(), Some("restart lev serve"));
                let daemon = link.daemon.expect("it said who it is");
                assert_eq!(daemon.build, "test-build");
                assert!(daemon.pid > 0);
                assert!(daemon.version.len() > 1);
            }
            (
                MachineEventType::ConfigHealthChanged,
                MachineEventFrame::ConfigHealthChanged(health),
            ) => {
                assert!(health.healthy);
                assert_eq!(health.path, "/config.toml");
                assert!(health.error.is_none());
                assert_eq!(health.config_mtime.map(|at| at.0), Some(1_788_000_000));
            }
            (MachineEventType::UpdateStepChanged, MachineEventFrame::UpdateStepChanged(step)) => {
                assert_eq!(step.job_id.as_str(), "job-1");
                assert_eq!(step.step, UpdateStep::Binary);
                assert_eq!(step.status, UpdateStepStatus::Running);
                assert_eq!(step.detail, "downloading");
            }
            (MachineEventType::UpdateFinished, MachineEventFrame::UpdateFinished(finished)) => {
                assert_eq!(finished.job_id.as_str(), "job-1");
                assert_eq!(finished.status, UpdateJobStatus::Complete);
                assert!(finished.restart_required);
                assert_eq!(finished.job.steps.len(), 1);
            }
            (kind, _) => panic!("{kind:?} converted to another type"),
        }
    }
}

/// The link frame is the one the daemon sends that both subscriptions carry.
#[test]
fn the_link_frame_reaches_a_run_subscription_too() {
    let link = ServerEvent::DaemonLink {
        connected: false,
        daemon: None,
        restarted: false,
        restart_advised: None,
    };
    let frame = run_frame(stamped(link)).expect("the link frame is a run frame too");
    match frame {
        RunEventFrame::DaemonLinkChanged(link) => {
            assert!(!link.connected);
            assert!(link.daemon.is_none());
        }
        _ => panic!("the link frame converts to the link frame"),
    }
}

/// The update frames are not a run's business, and the config one is not
/// either.
#[test]
fn the_installs_frames_are_not_run_frames() {
    for frame in [
        ServerEvent::ConfigHealth {
            healthy: true,
            path: "/config.toml".to_string(),
            error: None,
            config_mtime: None,
        },
        ServerEvent::UpdateProgress {
            job_id: "job-1".to_string(),
            step: update_job::Step::Keys,
            status: update_job::StepStatus::Skipped,
            detail: String::new(),
        },
        ServerEvent::UpdateFinished {
            job_id: "job-1".to_string(),
            status: update_job::JobStatus::Failed,
            restart_required: false,
            job: job_record(),
        },
    ] {
        assert!(run_frame(stamped(frame)).is_none());
    }
}

/// A subscription on one job sees that job's frames and nobody else's.
#[test]
fn one_jobs_subscription_sees_only_that_job() {
    let progress = ServerEvent::UpdateProgress {
        job_id: "job-1".to_string(),
        step: update_job::Step::Migrations,
        status: update_job::StepStatus::Advised,
        detail: "by hand".to_string(),
    };
    match update_job_frame(stamped(progress), "job-1").expect("its own frame") {
        UpdateJobEventFrame::UpdateStepChanged(step) => {
            assert_eq!(step.step, UpdateStep::Migrations);
            assert_eq!(step.status, UpdateStepStatus::Advised);
        }
        _ => panic!("a step frame converts to a step frame"),
    }
    let finished = ServerEvent::UpdateFinished {
        job_id: "job-1".to_string(),
        status: update_job::JobStatus::Complete,
        restart_required: false,
        job: job_record(),
    };
    match update_job_frame(stamped(finished), "job-1").expect("its own frame") {
        UpdateJobEventFrame::UpdateFinished(done) => {
            assert_eq!(done.status, UpdateJobStatus::Complete);
            assert!(!done.restart_required);
        }
        _ => panic!("a finish frame converts to a finish frame"),
    }
    // Another job's frames, and a frame about a run, are not this
    // subscription's business.
    for other in [
        ServerEvent::UpdateProgress {
            job_id: "job-2".to_string(),
            step: update_job::Step::Binary,
            status: update_job::StepStatus::Running,
            detail: String::new(),
        },
        ServerEvent::UpdateFinished {
            job_id: "job-2".to_string(),
            status: update_job::JobStatus::Failed,
            restart_required: false,
            job: job_record(),
        },
        ServerEvent::Log {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            line: "unrelated".to_string(),
        },
    ] {
        assert!(update_job_frame(stamped(other), "job-1").is_none());
    }
}

/// Every step and step status a job can report has a value here.
#[test]
fn every_update_step_and_status_maps_to_one_value() {
    let steps = [
        (update_job::Step::Binary, UpdateStep::Binary),
        (update_job::Step::Agents, UpdateStep::Blueprints),
        (update_job::Step::Keys, UpdateStep::Keys),
        (update_job::Step::Migrations, UpdateStep::Migrations),
    ];
    for (core, expected) in steps {
        let frame = ServerEvent::UpdateProgress {
            job_id: "job-1".to_string(),
            step: core,
            status: update_job::StepStatus::Pending,
            detail: String::new(),
        };
        match machine_frame(stamped(frame)).expect("a machine frame") {
            MachineEventFrame::UpdateStepChanged(step) => assert_eq!(step.step, expected),
            _ => panic!("a step frame converts to a step frame"),
        }
    }
    let statuses = [
        (update_job::StepStatus::Pending, UpdateStepStatus::Pending),
        (update_job::StepStatus::Running, UpdateStepStatus::Running),
        (update_job::StepStatus::Done, UpdateStepStatus::Done),
        (update_job::StepStatus::Skipped, UpdateStepStatus::Skipped),
        (update_job::StepStatus::Advised, UpdateStepStatus::Advised),
        (update_job::StepStatus::Failed, UpdateStepStatus::Failed),
    ];
    for (core, expected) in statuses {
        let frame = ServerEvent::UpdateProgress {
            job_id: "job-1".to_string(),
            step: update_job::Step::Binary,
            status: core,
            detail: String::new(),
        };
        match machine_frame(stamped(frame)).expect("a machine frame") {
            MachineEventFrame::UpdateStepChanged(step) => assert_eq!(step.status, expected),
            _ => panic!("a step frame converts to a step frame"),
        }
    }
    let jobs = [
        (update_job::JobStatus::Running, UpdateJobStatus::Running),
        (update_job::JobStatus::Complete, UpdateJobStatus::Complete),
        (update_job::JobStatus::Failed, UpdateJobStatus::Failed),
    ];
    for (core, expected) in jobs {
        let frame = ServerEvent::UpdateFinished {
            job_id: "job-1".to_string(),
            status: core,
            restart_required: false,
            job: job_record(),
        };
        match machine_frame(stamped(frame)).expect("a machine frame") {
            MachineEventFrame::UpdateFinished(done) => assert_eq!(done.status, expected),
            _ => panic!("a finish frame converts to a finish frame"),
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
    match run_frame(stamped(frame)).expect("a run frame") {
        RunEventFrame::InteractionOpened(opened) => {
            assert!(opened.interaction.is_none());
            assert_eq!(opened.run_id.as_str(), "run-a");
        }
        _ => panic!("an interaction frame converts to an interaction frame"),
    }
}

/// A status word this build has no state for is `UNKNOWN`, not a dropped
/// frame.
///
/// The daemon on the other end of the socket can be a newer build, and one new
/// state it sends must not cost a subscriber the whole frame.
#[test]
fn a_status_this_build_does_not_know_is_unknown() {
    assert_eq!(RunStatus::from_wire("running"), RunStatus::Running);
    assert_eq!(RunStatus::from_wire("cancelled"), RunStatus::Cancelled);
    assert_eq!(RunStatus::from_wire("hibernating"), RunStatus::Unknown);
    let frame = ServerEvent::AgentCompleted {
        agent_id: "a".to_string(),
        run_id: "run-a".to_string(),
        status: "hibernating".to_string(),
        result: Some("boom".to_string()),
        final_output: None,
    };
    match run_frame(stamped(frame)).expect("a run frame") {
        RunEventFrame::RunCompleted(completed) => {
            assert_eq!(completed.status, RunStatus::Unknown);
            assert_eq!(completed.error.as_deref(), Some("boom"));
            assert!(completed.final_output.is_none());
        }
        _ => panic!("a completion frame converts to a completion frame"),
    }
}

/// Why the config does not load reaches a subscriber as the whole diagnosis,
/// not one line of it.
#[test]
fn a_config_failure_carries_its_whole_diagnosis() {
    let frame = ServerEvent::ConfigHealth {
        healthy: false,
        path: "/config.toml".to_string(),
        error: Some(crate::commands::serve::config_types::ConfigErrorInfo {
            kind: "parse".to_string(),
            path: "/config.toml".to_string(),
            message: "expected a table".to_string(),
            line: Some(3),
            column: Some(1),
            key: Some("model_providers.local".to_string()),
            since: 1_788_000_000,
            note: "the file did not parse".to_string(),
        }),
        config_mtime: None,
    };
    match machine_frame(stamped(frame)).expect("a machine frame") {
        MachineEventFrame::ConfigHealthChanged(health) => {
            assert!(!health.healthy);
            let error = health.error.expect("the diagnosis");
            assert_eq!(error.kind, ConfigErrorKind::Parse);
            assert_eq!(error.message, "expected a table");
            assert_eq!(error.line, Some(3));
            assert_eq!(error.column, Some(1));
            assert_eq!(error.key.as_deref(), Some("model_providers.local"));
            assert_eq!(error.since.0, 1_788_000_000);
            assert_eq!(error.note, "the file did not parse");
            assert!(health.config_mtime.is_none());
        }
        _ => panic!("a config frame converts to a config frame"),
    }
}

/// A sequence number past what a 64-bit signed count holds is clamped rather
/// than wrapped, and so is a gap too large to count.
#[test]
fn an_impossible_count_clamps_rather_than_wraps() {
    let frame = Stamped {
        seq: u64::MAX,
        at: 1,
        event: ServerEvent::Log {
            agent_id: "a".to_string(),
            run_id: "run-a".to_string(),
            line: "x".to_string(),
        },
    };
    match run_frame(frame).expect("a run frame") {
        RunEventFrame::LogLineWritten(line) => assert_eq!(line.seq.0, i64::MAX),
        _ => panic!("a log frame converts to a log frame"),
    }
    let gap = super::dropped(u64::MAX, u64::MAX);
    assert_eq!(gap.count.0, i64::MAX);
    assert_eq!(gap.seq.0, i64::MAX);
    assert!(gap.at.0 > 0);
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
