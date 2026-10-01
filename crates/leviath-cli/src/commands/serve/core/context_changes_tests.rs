//! Reading why a run's window changed back out of its run file.

use leviath_core::context_cause::ContextCause;
use leviath_runtime::spec::names::{ModelRef, RegionName};
use leviath_runtime::state::{
    EntryKind, EntryMeta, EntryState, MessageState, RegionState, RunEvent, RunState,
    ToolResultState, TransitionReason, TransitionRecord,
};

use super::super::run_file::tests::{garbage, recorded, step};
use super::{by_execution, read};

/// A plain entry of `tokens` tokens.
fn entry(text: &str, tokens: u32) -> EntryState {
    EntryState {
        text: text.to_string(),
        parts: Vec::new(),
        tokens,
        timestamp: 0,
        kind: EntryKind::Text,
        meta: EntryMeta::None,
        key: None,
        reasoning: None,
    }
}

/// Append `text` to the run's first region, counting its tokens.
fn append(state: &mut RunState, text: &str) {
    let region = &mut state.context.regions[0];
    region.entries.push(entry(text, 5));
    region.current_tokens += 5;
}

fn message() -> RunEvent {
    RunEvent::Message(MessageState {
        from: "user".into(),
        text: "hello".into(),
        region: None,
    })
}

fn finished(call_id: &str) -> RunEvent {
    RunEvent::ToolFinished {
        call_id: call_id.to_string(),
        result: ToolResultState {
            text: "ok".into(),
            is_error: false,
        },
        millis: 0,
    }
}

fn inference() -> RunEvent {
    RunEvent::Inference {
        attempt: "a1".into(),
        model: ModelRef::parse("anthropic/m").unwrap(),
        spend: Default::default(),
        finish_reason: None,
    }
}

#[tokio::test]
async fn each_change_is_named_by_the_one_cause_its_step_records() {
    crate::runstate::with_isolated_runs_dir_async("context-changes-read", |_d| async move {
        let run_id = recorded();
        // 1: a message lands.
        step(
            &run_id,
            10,
            vec![message(), RunEvent::Log("l".into())],
            |s| {
                append(s, "hello");
            },
        );
        // 2: a tool's result lands.
        step(&run_id, 20, vec![finished("c1")], |s| append(s, "ok"));
        // 3: a reply and a result in one step: no one cause.
        step(&run_id, 30, vec![inference(), finished("c2")], |s| {
            append(s, "both");
        });
        // 4: nothing names a cause.
        step(&run_id, 40, Vec::new(), |s| append(s, "quiet"));
        // 5: a message whose step changed only a region's budget.
        step(&run_id, 50, vec![message()], |s| {
            s.context.regions[0].max_tokens += 1;
        });
        // 6: a message whose step left the window alone.
        step(&run_id, 60, vec![message()], |s| s.cursor.iteration += 1);
        // 7: a move to another stage rewrites the region, keeping its first
        // entry, and adds a region of its own.
        step(&run_id, 70, Vec::new(), |s| {
            let region = &mut s.context.regions[0];
            region.entries.truncate(1);
            region.entries.push(entry("carried", 2));
            region.current_tokens = 7;
            s.context.regions.push(RegionState {
                name: RegionName::new("notes").unwrap(),
                max_tokens: 100,
                current_tokens: 3,
                needs_message_compaction: false,
                taint: None,
                entries: vec![entry("a note", 3)],
            });
            s.last_transition = Some(TransitionRecord {
                from: s.cursor.stage.clone(),
                to: s.cursor.stage.clone(),
                edge: None,
                reason: TransitionReason::Forced,
                visit: "v2".into(),
            });
        });
        // 8: an answer, and the region the move added goes away.
        step(
            &run_id,
            80,
            vec![RunEvent::Answered {
                id: "q".into(),
                answer: "yes".into(),
            }],
            |s| {
                s.context.regions.pop();
            },
        );
        // 9: two results in one step land, with no one execution to name.
        step(&run_id, 90, vec![finished("c3"), finished("c4")], |s| {
            append(s, "two");
        });

        let changes = read(&run_id).unwrap();
        let causes: Vec<(u64, ContextCause)> = changes
            .iter()
            .map(|c| (c.position, c.record.cause))
            .collect();
        assert_eq!(
            causes,
            vec![
                (1, ContextCause::Message),
                (2, ContextCause::ToolResult),
                (7, ContextCause::Transform),
                (8, ContextCause::Interaction),
                (9, ContextCause::ToolResult),
            ]
        );

        let first = &changes[0].record;
        assert_eq!(first.at, 10);
        assert_ne!(first.revision_before, first.revision_after);
        assert!(first.execution_id.is_none());
        let grew = &first.regions[0];
        assert_eq!(grew.entries_added, 1);
        assert_eq!(grew.entries_removed, 0);
        assert_eq!(grew.token_delta, 5);

        assert_eq!(changes[1].record.execution_id.as_deref(), Some("c1"));
        assert!(changes[4].record.execution_id.is_none());

        let moved = &changes[2].record.regions;
        assert_eq!(moved.len(), 2);
        let rewritten = &moved[0];
        assert_eq!(
            rewritten.entries_added, 1,
            "the kept first entry is not new"
        );
        assert_eq!(rewritten.entries_after, Some(2));
        assert_eq!(
            rewritten.entries_removed,
            rewritten.entries_before.unwrap() - 1
        );
        let added = &moved[1];
        assert_eq!(added.region, "notes");
        assert_eq!(added.tokens_before, Some(0));
        assert_eq!(added.entries_added, 1);

        let gone = &changes[3].record.regions[0];
        assert_eq!(gone.region, "notes");
        assert_eq!(gone.tokens_after, Some(0));
        assert_eq!(gone.token_delta, -3);
        assert_eq!(gone.entries_removed, 1);

        assert_eq!(by_execution(&run_id, "c1").unwrap().len(), 1);
        assert!(by_execution(&run_id, "c9").unwrap().is_empty());
        assert!(by_execution(&run_id, "").unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn a_run_with_no_file_changed_nothing_and_an_unreadable_one_is_an_error() {
    crate::runstate::with_isolated_runs_dir_async("context-changes-none", |_d| async move {
        assert!(read("ghost").unwrap().is_empty());
        garbage("broken", b"not a run file");
        let stepped = recorded();
        super::super::run_file::tests::bad_step(&stepped, 1);
        assert_eq!(read(&stepped).unwrap_err().code(), "INTERNAL");
        assert_eq!(read("broken").unwrap_err().code(), "INTERNAL");
    })
    .await;
}
