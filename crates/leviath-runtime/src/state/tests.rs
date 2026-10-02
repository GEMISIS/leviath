use super::context::*;
use super::*;
use leviath_core::JsonDoc;
use leviath_core::mime::Delivery;
use leviath_core::taint::TaintLevel;

use crate::spec::inputs::{InputValue, InputValues};
use crate::spec::names::{Digest, EdgeName, InputName, RegionName};

fn stage(s: &str) -> StageName {
    StageName::new(s).unwrap()
}

fn entry(text: &str) -> EntryState {
    EntryState {
        text: text.into(),
        parts: vec![],
        tokens: 3,
        timestamp: 10,
        kind: EntryKind::Text,
        meta: EntryMeta::None,
        key: None,
        reasoning: None,
    }
}

fn region(name: &str, entries: Vec<EntryState>) -> RegionState {
    RegionState {
        name: RegionName::new(name).unwrap(),
        max_tokens: 100,
        current_tokens: entries.len() as u32 * 3,
        needs_message_compaction: false,
        taint: None,
        entries,
    }
}

pub(crate) fn base() -> RunState {
    let context = ContextState {
        regions: vec![
            region("system", vec![entry("be good")]),
            region("conversation", vec![]),
        ],
        hidden: vec![],
        max_tokens: 1000,
    };
    RunState::initial(stage("plan"), context, true)
}

/// A state with every field set away from its default, so a codec that
/// drops or mangles any field fails the round trip.
pub(crate) fn busy() -> RunState {
    let mut s = base();
    s.seq = 7;
    s.status = RunStatus::Error("boom".into());
    s.cursor = Cursor {
        stage: stage("build"),
        visit: "v2".into(),
        iteration: 4,
    };
    s.phase = PipelinePhase::AwaitingChoice(vec![EdgeName::new("done").unwrap()]);
    s.accepts_messages = false;
    s.visits.insert(stage("plan"), 2);
    s.progress = StageProgress {
        total_tool_calls: 3,
        edits_by_path: [("a.rs".to_string(), 2)].into(),
        entry_region_digests: [("notes".to_string(), 99)].into(),
        stage_started_at: Some(5),
        stuck_fired: true,
        ..StageProgress::default()
    };
    s.ledger.push(StageRecord {
        stage: stage("plan"),
        status: StageStatus::Complete,
        entered: true,
        spend: Spend {
            prompt_tokens: 10,
            priced_usd: 0.25,
            ..Spend::default()
        },
        models: vec![crate::spec::names::ModelRef::parse("mock/gpt-mock").unwrap()],
        visits: vec![VisitRecord {
            id: "v1".into(),
            entered_at: 1,
            left_at: Some(2),
            spend: Spend::default(),
            clock: Clock {
                banked_secs: 1,
                since: None,
            },
        }],
        region_tokens: [("system".to_string(), 3)].into(),
        first_call_prompt_tokens: Some(10),
        runaway_warned: false,
        output_cap_raised: true,
        started_at: Some(1),
        ended_at: Some(2),
        clock: Clock::default(),
    });
    let call = ToolCallState {
        id: "c1".into(),
        name: "read_file".into(),
        args: JsonDoc::new(serde_json::json!({"path": "a.rs"})),
        thought_signature: Some("sig".into()),
    };
    s.context.regions[1].entries = vec![
        EntryState {
            kind: EntryKind::AssistantTurn(vec![call.clone()]),
            reasoning: Some("hmm".into()),
            ..entry("reading")
        },
        EntryState {
            kind: EntryKind::ToolResult {
                call_id: "c1".into(),
                tool: "read_file".into(),
                is_error: false,
            },
            parts: vec![
                PartState {
                    mime_type: "image/png".into(),
                    body: PartBody::Stored(BlobState {
                        digest: Digest::of(b"png"),
                        size: 3,
                        width: Some(1),
                        height: Some(1),
                        duration_ms: None,
                        tokens: 85,
                        stand_in: "[image]".into(),
                    }),
                    name: Some("a.png".into()),
                    deliver: Some(Delivery::Native),
                },
                PartState {
                    mime_type: "text/plain".into(),
                    body: PartBody::Inline("hi".into()),
                    name: None,
                    deliver: None,
                },
            ],
            ..entry("fn main() {}")
        },
        EntryState {
            meta: EntryMeta::ChecklistItem {
                id: 1,
                done: true,
                note: Some("ok".into()),
            },
            key: Some("k".into()),
            ..entry("item")
        },
    ];
    s.context.regions[1].taint = Some(TaintState {
        level: TaintLevel::Private,
        entries: vec![TaintLevel::Public],
    });
    s.context.hidden = vec![RegionName::new("system").unwrap()];
    s.pending = Some(PendingBatch {
        calls: vec![call],
        done: [(
            "c1".to_string(),
            ToolResultState {
                text: "ok".into(),
                is_error: false,
            },
        )]
        .into(),
    });
    s.fan_out = Some(FanOutState {
        stage: stage("split"),
        config: crate::spec::graph::FanOutDef::same_graph(stage("split")),
        max_workers: 2,
        queued: vec![WorkItemState {
            id: "i1".into(),
            inputs: InputValues(
                [(
                    InputName::new("topic").unwrap(),
                    InputValue::Text("x".into()),
                )]
                .into(),
            ),
        }],
        active: vec![("i0".into(), RunId::new("w-1").unwrap())],
        done: vec![("i2".into(), "fine".into())],
        failed: vec![("i3".into(), "bad".into())],
        paused: true,
        origin: crate::fanout::FanOutOrigin::Tool {
            call_id: "c9".into(),
        },
        parts: vec![crate::state::context::PartState {
            mime_type: "text/plain".into(),
            body: crate::state::context::PartBody::Inline("found".into()),
            name: Some("found.txt".into()),
            deliver: None,
        }],
    });
    s.inbox.push(MessageState {
        from: "user".into(),
        text: "hurry".into(),
        region: None,
    });
    s.interactions.push(OpenInteraction {
        id: "q1".into(),
        prompt: "ok?".into(),
        options: vec!["yes".into()],
    });
    s.totals = Totals {
        spend: Spend {
            completion_tokens: 5,
            ..Spend::default()
        },
        tool_calls: 1,
    };
    s.clock = Clock {
        banked_secs: 60,
        since: Some(100),
    };
    s.flags = Flags {
        modified_files: vec!["a.rs".into()],
        produced_output: true,
        ..Flags::default()
    };
    s.children.push(RunId::new("child-1").unwrap());
    s.title = Some("Fix it".into());
    s.final_output = Some(FinalOutputState {
        content: "done".into(),
        format: Some("markdown".into()),
        stage: stage("build"),
        submitted_at: 9,
        truncated: false,
        artifacts: Vec::new(),
    });
    s.wait_reason = Some(super::WaitState::Children(1));
    s.last_transition = Some(TransitionRecord {
        from: stage("plan"),
        to: stage("build"),
        edge: Some(EdgeName::new("next").unwrap()),
        reason: TransitionReason::ModelChoice,
        visit: "v2".into(),
    });
    s.point = super::PointProgress {
        cursor: 1,
        round: 2,
        asking: Some("## Plan".into()),
    };
    s
}

#[test]
fn a_busy_state_survives_the_binary_codec() {
    let s = busy();
    let bin = postcard::to_stdvec(&s).unwrap();
    assert_eq!(postcard::from_bytes::<RunState>(&bin).unwrap(), s);
}

#[test]
fn replaying_the_delta_between_two_states_reaches_the_second() {
    let a = base();
    let b = busy();
    let delta = StateDelta::between(&a, &b, 42, vec![RunEvent::Log("step".into())]);
    assert_eq!(delta.seq, 1);
    assert!(!delta.is_empty());
    let mut replayed = a.clone();
    delta.apply(&mut replayed);
    let mut expected = b.clone();
    expected.seq = 1;
    assert_eq!(replayed, expected);
    // and back again
    let back = StateDelta::between(&b, &a, 43, vec![]);
    let mut again = b.clone();
    back.apply(&mut again);
    let mut expected = a.clone();
    expected.seq = b.seq + 1;
    assert_eq!(again, expected);
}

#[test]
fn an_unchanged_state_gives_an_empty_delta() {
    let s = busy();
    let d = StateDelta::between(&s, &s, 1, vec![]);
    assert!(d.is_empty());
    assert!(d.changes.is_empty());
}

#[test]
fn a_growing_region_records_only_the_new_entries() {
    let a = base();
    let mut b = a.clone();
    b.context.regions[0].entries.push(entry("more"));
    let d = ContextDiff::between(&a.context, &b.context);
    match &d.regions[..] {
        [(name, _, Some(RegionChange::Append(new)))] => {
            assert_eq!(name.as_str(), "system");
            assert_eq!(new.len(), 1);
        }
        other => panic!("{other:?}"),
    }
    let mut c = a.context.clone();
    d.apply(&mut c);
    assert_eq!(c, b.context);
}

#[test]
fn a_region_whose_budget_alone_changed_keeps_its_entries() {
    let a = base();
    let mut b = a.clone();
    b.context.regions[0].max_tokens += 1;
    let d = ContextDiff::between(&a.context, &b.context);
    assert_eq!(d.regions.len(), 1);
    assert_eq!(d.regions[0].2, None);
    let mut c = a.context.clone();
    d.apply(&mut c);
    assert_eq!(c, b.context);
}

#[test]
fn regions_can_come_go_and_move() {
    let a = base();
    let mut b = a.clone();
    b.context.regions.reverse();
    b.context.regions.remove(0);
    b.context.regions.push(region("notes", vec![entry("n")]));
    b.context.regions.insert(0, region("fresh", vec![]));
    b.context.max_tokens = 5;
    let d = ContextDiff::between(&a.context, &b.context);
    assert_eq!(d.removed.len(), 1);
    assert!(d.order.is_some());
    let mut c = a.context.clone();
    d.apply(&mut c);
    assert_eq!(c, b.context);
    assert!(c.region("notes").is_some());
}

#[test]
fn a_ledger_record_changes_in_place_or_appends() {
    let mut a = busy();
    a.ledger.clear();
    let b = busy();
    let d = StateDelta::between(&a, &b, 0, vec![]);
    assert!(
        d.changes
            .iter()
            .any(|c| matches!(c, Change::LedgerRecord(0, _)))
    );
    let mut c = a.clone();
    d.apply(&mut c);
    assert_eq!(c.ledger, b.ledger);
    let mut edited = b.clone();
    edited.ledger[0].entered = false;
    let d2 = StateDelta::between(&b, &edited, 0, vec![]);
    let mut c2 = b.clone();
    d2.apply(&mut c2);
    assert_eq!(c2.ledger, edited.ledger);
}

#[test]
fn deltas_and_events_survive_the_binary_codec() {
    let call = ToolCallState {
        id: "c".into(),
        name: "t".into(),
        args: JsonDoc::default(),
        thought_signature: None,
    };
    let model = crate::spec::names::ModelRef::parse("a/b").unwrap();
    let d = StateDelta {
        seq: 3,
        at: 9,
        changes: vec![
            Change::Status(RunStatus::Paused),
            Change::Phase(PipelinePhase::Wedged("x".into())),
        ],
        events: vec![
            RunEvent::Inference {
                attempt: "a1".into(),
                model: model.clone(),
                spend: Spend::default(),
                finish_reason: Some("stop".into()),
            },
            RunEvent::Failover {
                from: model.clone(),
                to: model,
                reason: "429".into(),
            },
            RunEvent::ToolStarted(call),
            RunEvent::ToolFinished {
                call_id: "c".into(),
                result: ToolResultState {
                    text: "r".into(),
                    is_error: true,
                },
                millis: 5,
            },
            RunEvent::Answered {
                id: "q".into(),
                answer: "yes".into(),
            },
            RunEvent::Message(MessageState {
                from: "p".into(),
                text: "t".into(),
                region: Some("notes".into()),
            }),
            RunEvent::Log("l".into()),
        ],
    };
    let bin = postcard::to_stdvec(&d).unwrap();
    assert_eq!(postcard::from_bytes::<StateDelta>(&bin).unwrap(), d);
}
