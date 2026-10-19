use super::*;
use leviath_runtime::runfile::codec::{self, FrameKind};

/// A stage record as layout 7 wrote it.
fn old_record() -> StageRecord {
    StageRecord {
        stage: StageName::new("s").unwrap(),
        status: StageStatus::Complete,
        entered: true,
        spend: Spend {
            prompt_tokens: 12,
            ..Default::default()
        },
        models: Vec::new(),
        visits: Vec::new(),
        region_tokens: BTreeMap::from([("conversation".to_string(), 7)]),
        first_call_prompt_tokens: Some(12),
        runaway_warned: false,
        output_cap_raised: true,
        started_at: Some(1),
        ended_at: Some(2),
        clock: Default::default(),
    }
}

/// `value` written as a delta frame, and read back as `T`.
fn through<T: serde::de::DeserializeOwned>(value: &impl serde::Serialize) -> T {
    let written = codec::encode(FrameKind::Delta, value).unwrap();
    codec::frame_at(&written, 0)
        .unwrap()
        .decode(&written)
        .unwrap()
}

/// A layout-7 stage record reads as this build's with nothing else changed
/// and no resubmissions counted.
#[test]
fn a_stage_record_comes_through_with_no_resubmissions() {
    let read: StageRecord = through(&old_record());
    let record = state::StageRecord::from(read);
    assert_eq!(record.resubmissions, 0);
    assert_eq!(record.spend.prompt_tokens, 12);
    assert_eq!(record.region_tokens["conversation"], 7);
    assert!(record.output_cap_raised);
    assert_eq!((record.started_at, record.ended_at), (Some(1), Some(2)));
}

/// Every change but a ledger record's is the same in both layouts: written as
/// this build writes it, read as layout 7 and converted, it is the same bytes.
/// This is also what holds the mirror's variants in this build's order. A
/// ledger record, the one that changed, reads as this build's with no
/// resubmissions.
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
}

/// A layout-7 delta, and the state it leaves, read as this build's.
#[test]
fn a_delta_and_a_state_come_through() {
    let delta = state::StateDelta {
        seq: 3,
        at: 9,
        changes: vec![state::Change::Written(5)],
        events: vec![RunEvent::Log("line".into())],
    };
    let read: StateDelta = through(&delta);
    assert_eq!(state::StateDelta::from(read), delta);

    let mut run =
        state::RunState::initial(StageName::new("s").unwrap(), ContextState::default(), false);
    run.ledger.push(old_record().into());
    let read: RunState = through(&old_state(&run));
    assert_eq!(state::RunState::from(read), run);
}

/// `run` in layout 7's shape: every field in order, its ledger records
/// without the count.
fn old_state(run: &state::RunState) -> impl serde::Serialize + '_ {
    let ledger: Vec<StageRecord> = run.ledger.iter().map(|_| old_record()).collect();
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
            &run.totals,
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
