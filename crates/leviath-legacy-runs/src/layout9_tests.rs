use super::*;
use crate::layout8::tests::through;
use leviath_runtime::runfile::codec::{self, FrameKind};
use leviath_runtime::state::journal::{CauseState, QuestionKind, RequestDigestState};

/// A failure as layout 9 wrote it, and as this build reads it: the same
/// sentence, and no kind.
fn failed() -> (RunStatus, state::RunStatus) {
    (
        RunStatus::Error("boom".into()),
        state::RunStatus::Error {
            message: "boom".into(),
            kind: None,
        },
    )
}

/// Every status layout 9 wrote reads as this build's, and a failure keeps
/// its sentence and states no kind.
#[test]
fn every_status_comes_through() {
    use state::RunStatus as S;
    let pairs = [
        (RunStatus::Idle, S::Idle),
        (RunStatus::Active, S::Active),
        (RunStatus::Waiting, S::Waiting),
        (RunStatus::Paused, S::Paused),
        (RunStatus::Complete, S::Complete),
        failed(),
        (RunStatus::Cancelled, S::Cancelled),
    ];
    for (old, new) in pairs {
        assert_eq!(S::from(through::<RunStatus>(&old)), new);
    }
}

/// Every change but a status is the same in both layouts: written as this
/// build writes it, read as layout 9 and converted, it is the same bytes.
/// This is also what holds the mirror's variants in this build's order. A
/// status reads as this build's.
#[test]
fn every_change_comes_through() {
    use state::Change as C;
    let stage = StageName::new("s").unwrap();
    let changes: Vec<C> = vec![
        C::Status(state::RunStatus::Complete),
        C::Cursor(Cursor {
            stage: stage.clone(),
            visit: "v".into(),
            iteration: 1,
        }),
        C::Phase(PipelinePhase::Done),
        C::AcceptsMessages(true),
        C::Visits(BTreeMap::new()),
        C::Progress(Default::default()),
        C::LedgerRecord(
            1,
            crate::layout8::StageRecord {
                stage: stage.clone(),
                status: state::StageStatus::Complete,
                entered: true,
                spend: Default::default(),
                models: Vec::new(),
                visits: Vec::new(),
                region_tokens: BTreeMap::new(),
                first_call_prompt_tokens: Some(12),
                runaway_warned: false,
                output_cap_raised: true,
                resubmissions: 2,
                started_at: Some(1),
                ended_at: Some(2),
                clock: Default::default(),
            }
            .into(),
        ),
        C::LedgerTruncate(2),
        C::Context(Default::default()),
        C::Pending(None),
        C::FanOut(None),
        C::Inbox(Vec::new()),
        C::Interactions(Vec::new()),
        C::Totals(Default::default()),
        C::Clock(Default::default()),
        C::Flags(Default::default()),
        C::Children(vec![RunId::new("run-c").unwrap()]),
        C::Title(Some("t".into())),
        C::FinalOutput(None),
        C::WaitReason(None),
        C::LastTransition(None),
        C::TitleError(Some("e".into())),
        C::ReadPaths(None),
        C::Point(Default::default()),
        C::Held(None),
        C::Files(Default::default()),
        C::Blobs(Vec::new()),
        C::Grants(Default::default()),
        C::Written(3),
        C::LastProgressAt(Some(4)),
        C::RemoteJobs(BTreeMap::new()),
        C::OpenCalls(Vec::new()),
    ];
    for change in changes {
        let written = codec::encode(FrameKind::Delta, &change).unwrap();
        let back = state::Change::from(through::<Change>(&change));
        let again = codec::encode(FrameKind::Delta, &back).unwrap();
        assert!(again == written, "{back:?}");
    }

    let (old, new) = failed();
    let read: Change = through(&Change::Status(old));
    assert_eq!(state::Change::from(read), state::Change::Status(new));
}

/// Every event layout 9 wrote reads as this build's. All but a segment
/// starting are the same bytes; that one holds how the segment before it
/// ended, whose status reads as this build's.
#[test]
fn every_event_comes_through() {
    use state::RunEvent as E;
    let model = ModelRef::parse("mock/m").unwrap();
    let stage = StageName::new("s").unwrap();
    let unchanged: Vec<E> = vec![
        E::Inference {
            attempt: "a1".into(),
            model: model.clone(),
            spend: Default::default(),
            finish_reason: Some("stop".into()),
            kind: CallKind::Routing,
            stage: Some(stage.clone()),
            iteration: 3,
            ending: CallEnding::Answered,
            provider_call_id: Some("gen-1".into()),
        },
        E::Transition(TransitionRecord {
            from: stage.clone(),
            to: stage.clone(),
            edge: None,
            reason: state::TransitionReason::Forced,
            visit: "v".into(),
        }),
        E::Failover {
            from: model.clone(),
            to: model.clone(),
            reason: "credits".into(),
        },
        E::ToolStarted(ToolCallState {
            id: "c1".into(),
            name: "shell".into(),
            args: leviath_core::JsonDoc::new(serde_json::json!({ "command": "ls" })),
            thought_signature: None,
        }),
        E::ToolFinished {
            call_id: "c1".into(),
            result: ToolResultState {
                text: "ok".into(),
                is_error: false,
            },
            millis: 4,
        },
        E::Answered {
            id: "q1".into(),
            answer: "yes".into(),
        },
        E::Message(MessageState {
            from: "user".into(),
            text: "hi".into(),
            region: None,
        }),
        E::Log("line".into()),
        E::Attempt(Box::new(AttemptState {
            id: "a1".into(),
            number: 2,
            provider: "mock".into(),
            model: "m".into(),
            outcome: leviath_runtime::state::journal::AttemptOutcomeState::Succeeded,
            finish_reason: None,
            stopped_for: None,
            duration_ms: 5,
            backoff_ms: 6,
            digest: RequestDigestState {
                system_hash: 1,
                messages: 2,
                tools: 3,
                max_tokens: 4,
                temperature: 0.5,
            },
            model_input: None,
        })),
        E::Dispatched {
            call_id: "c1".into(),
            execution_id: "e1".into(),
            requested_by: "a1".into(),
        },
        E::Completed {
            call_id: "c1".into(),
            execution_id: "e1".into(),
            outcome: Some(ToolOutcomeState::Succeeded),
            parts: vec!["p".into()],
        },
        E::Artifacts {
            execution_id: "e1".into(),
            artifacts: Vec::new(),
        },
        E::Settled(Box::new(SettledState {
            id: "q1".into(),
            kind: QuestionKind::Confirm,
            tool: None,
            prompt: "ok?".into(),
            stage: "s".into(),
            settlement: "{}".into(),
            asked_at: 1,
        })),
        E::ContextCommitted(Box::new(ContextCommitState {
            cause: CauseState::Seed,
            execution_id: None,
            revision_before: "a".into(),
            revision_after: "b".into(),
            regions: Vec::new(),
        })),
        E::ContextNoted(ContextNoteState {
            region: "r".into(),
            cause: CauseState::Seed,
            entries_added: 1,
            entries_removed: 0,
            token_delta: 5,
        }),
        E::Continued(Box::new(state::ContinuedState {
            segment: 2,
            stage: stage.clone(),
            forked_from: None,
            previous: None,
            regions: Vec::new(),
        })),
    ];
    for event in unchanged {
        let written = codec::encode(FrameKind::Delta, &event).unwrap();
        let back = state::RunEvent::from(through::<RunEvent>(&event));
        let again = codec::encode(FrameKind::Delta, &back).unwrap();
        assert!(again == written, "{back:?}");
    }

    let (old, new) = failed();
    let continued = RunEvent::Continued(Box::new(ContinuedState {
        segment: 2,
        stage: stage.clone(),
        forked_from: None,
        previous: Some(SegmentEnd {
            status: old,
            stage,
            spend: Default::default(),
            tool_calls: 7,
            final_output: None,
            file: None,
            seq: 11,
        }),
        regions: Vec::new(),
    }));
    let E::Continued(read) = state::RunEvent::from(through::<RunEvent>(&continued)) else {
        unreachable!()
    };
    let previous = read.previous.expect("the segment before");
    assert_eq!(previous.status, new);
    assert_eq!((previous.tool_calls, previous.seq), (7, 11));
}

/// A layout-9 delta, and the state it leaves, read as this build's: every
/// field as it was, and the failure with its sentence and no kind.
#[test]
fn a_delta_and_a_state_come_through() {
    let (old, new) = failed();
    let delta = StateDelta {
        seq: 3,
        at: 9,
        changes: vec![Change::Written(5), Change::Status(old.clone())],
        events: vec![RunEvent::Log("line".into())],
    };
    assert_eq!(
        state::StateDelta::from(through::<StateDelta>(&delta)),
        state::StateDelta {
            seq: 3,
            at: 9,
            changes: vec![
                state::Change::Written(5),
                state::Change::Status(new.clone())
            ],
            events: vec![state::RunEvent::Log("line".into())],
        }
    );

    let mut run =
        state::RunState::initial(StageName::new("s").unwrap(), ContextState::default(), false);
    run.title = Some("t".into());
    run.written = 4;
    let s = run.clone();
    let as_written = RunState {
        seq: s.seq,
        status: old,
        cursor: s.cursor,
        phase: s.phase,
        accepts_messages: s.accepts_messages,
        visits: s.visits,
        progress: s.progress,
        ledger: s.ledger,
        context: s.context,
        pending: s.pending,
        fan_out: s.fan_out,
        inbox: s.inbox,
        interactions: s.interactions,
        totals: s.totals,
        clock: s.clock,
        flags: s.flags,
        children: s.children,
        title: s.title,
        final_output: s.final_output,
        wait_reason: s.wait_reason,
        last_transition: s.last_transition,
        point: s.point,
        title_error: s.title_error,
        read_paths: s.read_paths,
        held: s.held,
        files: s.files,
        blobs: s.blobs,
        grants: s.grants,
        written: s.written,
        last_progress_at: s.last_progress_at,
        remote_jobs: s.remote_jobs,
        open_calls: s.open_calls,
    };
    run.status = new;
    assert_eq!(state::RunState::from(through::<RunState>(&as_written)), run);

    // And a state this build writes with any other status is layout 9's
    // bytes: the two differ in a failure alone.
    run.status = state::RunStatus::Paused;
    let written = codec::encode(FrameKind::State, &run).unwrap();
    let back = state::RunState::from(through::<RunState>(&run));
    assert!(codec::encode(FrameKind::State, &back).unwrap() == written);
}
