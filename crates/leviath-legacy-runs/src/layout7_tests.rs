use super::*;
use crate::layout8::tests::{old_spend, old_state, through};
use leviath_runtime::state::ContextState;

/// A stage record as layout 7 wrote it.
fn old_record() -> StageRecord {
    StageRecord {
        stage: StageName::new("s").unwrap(),
        status: StageStatus::Complete,
        entered: true,
        spend: old_spend(0, false),
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

/// A ledger change, a delta and a state in layout 7 read as this build's:
/// layout 8's, holding this layout's stage record.
#[test]
fn a_change_a_delta_and_a_state_hold_this_layouts_record() {
    // The variant's index, then its fields, as postcard writes an enum.
    let read: Change = through(&(6u32, 1u32, old_record()));
    let expected = state::Change::LedgerRecord(1, old_record().into());
    assert_eq!(state::Change::from(read), expected);

    let delta = (
        3u64,
        9i64,
        vec![(6u32, 0u32, old_record())],
        Vec::<leviath_runtime::state::RunEvent>::new(),
    );
    let read = state::StateDelta::from(through::<StateDelta>(&delta));
    assert_eq!(read.seq, 3);
    assert_eq!(
        read.changes,
        vec![state::Change::LedgerRecord(0, old_record().into())]
    );

    let mut run =
        state::RunState::initial(StageName::new("s").unwrap(), ContextState::default(), false);
    run.ledger.push(old_record().into());
    let read: RunState = through(&old_state(&run, vec![old_record()]));
    let read = state::RunState::from(read);
    assert_eq!(read.ledger, run.ledger);
    assert_eq!(read.totals.tool_calls, 6);
}
