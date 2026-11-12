use super::*;
use leviath_runtime::runfile::codec::{self, FrameKind};
use leviath_runtime::state::journal::{CauseState, QuestionKind};

/// `value` written as a delta frame, and read back as `T`.
pub(crate) fn through<T: serde::de::DeserializeOwned>(value: &impl serde::Serialize) -> T {
    let written = codec::encode(FrameKind::Delta, value).unwrap();
    codec::frame_at(&written, 0)
        .unwrap()
        .decode(&written)
        .unwrap()
}

/// A spend as layout 8 wrote it.
pub(crate) fn old_spend(unpriced_calls: u32, cost_unknown: bool) -> Spend {
    Spend {
        prompt_tokens: 12,
        completion_tokens: 3,
        cached_tokens: 2,
        cache_write_tokens: 1,
        priced_usd: 0.5,
        reported_calls: 4,
        computed_calls: 5,
        unpriced_calls,
        cost_unknown,
    }
}

/// A stage record as layout 8 wrote it.
fn old_record() -> StageRecord {
    StageRecord {
        stage: StageName::new("s").unwrap(),
        status: StageStatus::Complete,
        entered: true,
        spend: old_spend(1, false),
        models: Vec::new(),
        visits: vec![VisitRecord {
            id: "v1".into(),
            entered_at: 1,
            left_at: Some(2),
            spend: old_spend(0, true),
            clock: Default::default(),
        }],
        region_tokens: BTreeMap::from([("conversation".to_string(), 7)]),
        first_call_prompt_tokens: Some(12),
        runaway_warned: false,
        output_cap_raised: true,
        resubmissions: 2,
        started_at: Some(1),
        ended_at: Some(2),
        clock: Default::default(),
    }
}

/// A layout-8 spend reads as this build's: its tokens and its priced calls
/// as they were, its unpriced calls unknown for want of a price, and a cost
/// its record never named as one call recorded without one.
#[test]
fn a_spend_keeps_its_figures_and_says_why_a_cost_is_unknown() {
    let known = state::Spend::from(through::<Spend>(&old_spend(0, false)));
    assert_eq!((known.prompt_tokens, known.completion_tokens), (12, 3));
    assert_eq!((known.cached_tokens, known.cache_write_tokens), (2, 1));
    assert_eq!(known.priced_usd, 0.5);
    assert_eq!((known.reported_calls, known.computed_calls), (4, 5));
    assert!(known.is_exact());
    assert_eq!(known.cost_usd(), Some(0.5));

    let unpriced = state::Spend::from(old_spend(3, false));
    assert_eq!(unpriced.unknown.no_price, 3);
    assert_eq!(unpriced.unknown_calls(), 3);
    assert_eq!(unpriced.cost_usd(), None);

    let never_named = state::Spend::from(old_spend(0, true));
    assert_eq!(never_named.unknown.not_recorded, 1);
    assert_eq!(never_named.unknown_calls(), 1);

    // Both said: the unpriced calls are the reason, counted once.
    let both = state::Spend::from(old_spend(2, true));
    assert_eq!((both.unknown.no_price, both.unknown.not_recorded), (2, 0));
}

/// A layout-8 stage record reads as this build's, its spend and each
/// visit's converted and nothing else changed.
#[test]
fn a_stage_record_comes_through_with_its_spends_converted() {
    let record = state::StageRecord::from(through::<StageRecord>(&old_record()));
    assert_eq!(record.resubmissions, 2);
    assert_eq!(record.spend.unknown.no_price, 1);
    assert_eq!(record.visits[0].spend.unknown.not_recorded, 1);
    assert_eq!(record.visits[0].id, "v1");
    assert_eq!(record.region_tokens["conversation"], 7);
    assert!(record.output_cap_raised);
    assert_eq!((record.started_at, record.ended_at), (Some(1), Some(2)));
}

/// Every change but a ledger record's and the totals' is the same in both
/// layouts: written as this build writes it, read as layout 8 and converted,
/// it is the same bytes. This is also what holds the mirror's variants in
/// this build's order. The two that hold a spend read as this build's with
/// the spend converted.
#[test]
fn every_change_comes_through() {
    use state::Change as C;
    let stage = StageName::new("s").unwrap();
    let changes: Vec<C> = vec![
        C::Status(RunStatus::Complete),
        C::Cursor(Cursor {
            stage,
            visit: "v".into(),
            iteration: 1,
        }),
        C::Phase(PipelinePhase::Done),
        C::AcceptsMessages(true),
        C::Visits(BTreeMap::new()),
        C::Progress(Default::default()),
        C::LedgerTruncate(2),
        C::Context(Default::default()),
        C::Pending(None),
        C::FanOut(None),
        C::Inbox(Vec::new()),
        C::Interactions(Vec::new()),
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
    ];
    for change in changes {
        let written = codec::encode(FrameKind::Delta, &change).unwrap();
        let read: Change = through(&change);
        let back = state::Change::from(read);
        let again = codec::encode(FrameKind::Delta, &back).unwrap();
        assert!(again == written, "{back:?}");
    }

    // The variant's index, then its fields, as postcard writes an enum.
    let read: Change = through(&(6u32, 1u32, old_record()));
    let expected = state::Change::LedgerRecord(1, old_record().into());
    assert_eq!(state::Change::from(read), expected);

    let totals = || Totals {
        spend: old_spend(1, false),
        tool_calls: 9,
    };
    let read: Change = through(&(13u32, totals()));
    let state::Change::Totals(converted) = state::Change::from(read) else {
        unreachable!()
    };
    assert_eq!(converted.tool_calls, 9);
    assert_eq!(converted.spend.unknown.no_price, 1);
}

/// Every event layout 8 wrote reads as this build's. The ones that hold
/// nothing layout 9 changed are the same bytes; a billed call was answered
/// and names no id of the provider's, a failed attempt does not say whether
/// its request went out, and a segment's ending has its spend converted.
#[test]
fn every_event_comes_through() {
    use state::RunEvent as E;
    let model = ModelRef::parse("mock/m").unwrap();
    let stage = StageName::new("s").unwrap();
    let unchanged: Vec<E> = vec![
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
    ];
    for event in unchanged {
        let written = codec::encode(FrameKind::Delta, &event).unwrap();
        let back = state::RunEvent::from(through::<RunEvent>(&event));
        let again = codec::encode(FrameKind::Delta, &back).unwrap();
        assert!(again == written, "{back:?}");
    }

    let billed = RunEvent::Inference {
        attempt: "a1".into(),
        model: model.clone(),
        spend: old_spend(1, false),
        finish_reason: Some("stop".into()),
        kind: CallKind::Routing,
        stage: Some(stage.clone()),
        iteration: 3,
    };
    let E::Inference {
        spend,
        ending,
        provider_call_id,
        iteration,
        ..
    } = state::RunEvent::from(through::<RunEvent>(&billed))
    else {
        unreachable!()
    };
    assert_eq!(spend.unknown.no_price, 1);
    assert_eq!(ending, CallEnding::Answered);
    assert_eq!(provider_call_id, None);
    assert_eq!(iteration, 3);

    let attempt = |outcome| AttemptState {
        id: "a1".into(),
        number: 2,
        provider: "mock".into(),
        model: "m".into(),
        outcome,
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
    };
    let failed = RunEvent::Attempt(Box::new(attempt(AttemptOutcomeState::Failed {
        kind: "timeout".into(),
        transient: true,
        capacity: false,
        next: RetryState::SameModel,
    })));
    let E::Attempt(read) = state::RunEvent::from(through::<RunEvent>(&failed)) else {
        unreachable!()
    };
    assert_eq!(
        read.outcome,
        journal::AttemptOutcomeState::Failed {
            kind: "timeout".into(),
            transient: true,
            capacity: false,
            next: RetryState::SameModel,
            sent: None,
        }
    );
    assert_eq!((read.number, read.duration_ms, read.backoff_ms), (2, 5, 6));
    let answered = RunEvent::Attempt(Box::new(attempt(AttemptOutcomeState::Succeeded)));
    let E::Attempt(read) = state::RunEvent::from(through::<RunEvent>(&answered)) else {
        unreachable!()
    };
    assert_eq!(read.outcome, journal::AttemptOutcomeState::Succeeded);

    let continued = |previous| {
        RunEvent::Continued(Box::new(ContinuedState {
            segment: 2,
            stage: stage.clone(),
            forked_from: None,
            previous,
            regions: Vec::new(),
        }))
    };
    let ended = SegmentEnd {
        status: RunStatus::Complete,
        stage: stage.clone(),
        spend: old_spend(0, true),
        tool_calls: 7,
        final_output: None,
        file: None,
        seq: 11,
    };
    let E::Continued(read) = state::RunEvent::from(through::<RunEvent>(&continued(Some(ended))))
    else {
        unreachable!()
    };
    let previous = read.previous.expect("the segment before");
    assert_eq!(previous.spend.unknown.not_recorded, 1);
    assert_eq!((previous.tool_calls, previous.seq), (7, 11));
    let E::Continued(fork) = state::RunEvent::from(through::<RunEvent>(&continued(None))) else {
        unreachable!()
    };
    assert_eq!((fork.segment, fork.previous), (2, None));
}

/// `run` in layout 8's shape: every field in order, with `ledger` as its
/// stage records and layout 8's totals.
pub(crate) fn old_state<'a, R: serde::Serialize + 'a>(
    run: &'a state::RunState,
    ledger: Vec<R>,
) -> impl serde::Serialize + 'a {
    (
        (
            run.seq,
            &run.status,
            &run.cursor,
            &run.phase,
            run.accepts_messages,
            &run.visits,
            &run.progress,
            ledger,
            &run.context,
            &run.pending,
            &run.fan_out,
            &run.inbox,
            &run.interactions,
            Totals {
                spend: old_spend(0, false),
                tool_calls: 6,
            },
            &run.clock,
            &run.flags,
        ),
        (
            &run.children,
            &run.title,
            &run.final_output,
            &run.wait_reason,
            &run.last_transition,
            &run.point,
            &run.title_error,
            &run.read_paths,
            &run.held,
            &run.files,
            &run.blobs,
            &run.grants,
            run.written,
            run.last_progress_at,
            &run.remote_jobs,
        ),
    )
}

/// A layout-8 delta, and the state it leaves, read as this build's: the
/// state lists no call as out.
#[test]
fn a_delta_and_a_state_come_through() {
    let delta = (
        3u64,
        9i64,
        vec![state::Change::Written(5)],
        vec![RunEvent::Log("line".into())],
    );
    let read = state::StateDelta::from(through::<StateDelta>(&delta));
    assert_eq!(
        read,
        state::StateDelta {
            seq: 3,
            at: 9,
            changes: vec![state::Change::Written(5)],
            events: vec![state::RunEvent::Log("line".into())],
        }
    );

    let mut run =
        state::RunState::initial(StageName::new("s").unwrap(), ContextState::default(), false);
    run.ledger.push(old_record().into());
    run.totals = Totals {
        spend: old_spend(0, false),
        tool_calls: 6,
    }
    .into();
    let read: RunState = through(&old_state(&run, vec![old_record()]));
    let read = state::RunState::from(read);
    assert!(read.open_calls.is_empty());
    assert_eq!(read, run);
}
