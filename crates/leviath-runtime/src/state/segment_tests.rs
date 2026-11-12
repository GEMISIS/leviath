use super::*;

#[test]
fn a_segment_ends_with_the_run_as_it_stood() {
    let mut state = crate::runfile::reader_tests::scripted_run(2).pop().unwrap();
    state.status = RunStatus::Error("boom".into());
    state.totals.spend.priced_usd = 2.0;
    state.totals.tool_calls = 7;
    let file = FileRef::whole(kept_answer_path(1), b"done");
    let end = SegmentEnd::of(&state, Some(file.clone()));
    assert_eq!(end.status, RunStatus::Error("boom".into()));
    assert_eq!(end.stage, state.cursor.stage);
    assert_eq!(end.spend.priced_usd, 2.0);
    assert_eq!(end.tool_calls, 7);
    assert_eq!(end.file, Some(file));
    assert_eq!(end.seq, state.seq);
    assert_eq!(kept_answer_path(3), "segments/3/final_output");
}

#[test]
fn what_a_segment_spent_is_what_the_run_spent_since_it_started() {
    let before = Spend {
        prompt_tokens: 10,
        completion_tokens: 5,
        cached_tokens: 2,
        cache_write_tokens: 1,
        priced_usd: 1.0,
        reported_calls: 1,
        computed_calls: 1,
        unknown: Default::default(),
    };
    let now = Spend {
        prompt_tokens: 25,
        completion_tokens: 9,
        cached_tokens: 2,
        cache_write_tokens: 4,
        priced_usd: 1.75,
        reported_calls: 3,
        computed_calls: 1,
        unknown: leviath_core::cost::UnknownCalls {
            no_lookup: 1,
            ..Default::default()
        },
    };
    let own = spent_since(&now, &before);
    assert_eq!(own.prompt_tokens, 15);
    assert_eq!(own.completion_tokens, 4);
    assert_eq!(own.cached_tokens, 0);
    assert_eq!(own.cache_write_tokens, 3);
    assert_eq!(own.priced_usd, 0.75);
    assert_eq!(own.reported_calls, 2);
    assert_eq!(own.computed_calls, 0);
    assert_eq!(own.unknown_calls(), 1);
    assert_eq!(own.unknown.no_lookup, 1, "with its reason");
    assert_eq!(own.cost_usd(), None);
    // Never below zero.
    assert_eq!(spent_since(&before, &now).priced_usd, 0.0);
}
