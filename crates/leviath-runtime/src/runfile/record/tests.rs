//! The records' wire names and the window fingerprint.

use super::*;
use leviath_core::run_meta::RegionSnapshot;

fn entry(content: &str, tokens: usize) -> RegionEntrySnapshot {
    RegionEntrySnapshot {
        content: content.into(),
        tokens,
        kind: leviath_core::region::EntryKind::Text,
        metadata: None,
        key: None,
        taint: Default::default(),
        reasoning: None,
    }
}

fn region(name: &str, entries: Vec<RegionEntrySnapshot>) -> RegionSnapshot {
    let current = entries.iter().map(|e| e.tokens).sum();
    RegionSnapshot {
        name: name.to_string(),
        kind: "clearable".to_string(),
        current_tokens: current,
        max_tokens: 1000,
        entries,
        description: None,
    }
}

fn snapshot(regions: Vec<RegionSnapshot>) -> ContextSnapshot {
    let total = regions.iter().map(|r| r.current_tokens).sum();
    ContextSnapshot {
        stage_name: "s1".to_string(),
        total_tokens: total,
        max_tokens: 10_000,
        regions,
    }
}

/// Two windows that hold the same thing fingerprint the same, and any change
/// moves it.
#[test]
fn the_fingerprint_follows_the_window() {
    let a = snapshot(vec![region("conv", vec![entry("hi", 1)])]);
    let same = snapshot(vec![region("conv", vec![entry("hi", 1)])]);
    let grown = snapshot(vec![region(
        "conv",
        vec![entry("hi", 1), entry("there", 2)],
    )]);
    let renamed = snapshot(vec![region("plan", vec![entry("hi", 1)])]);
    assert_eq!(context_fingerprint(&a).len(), 16);
    assert_eq!(context_fingerprint(&a), context_fingerprint(&same));
    assert_ne!(context_fingerprint(&a), context_fingerprint(&grown));
    assert_ne!(context_fingerprint(&a), context_fingerprint(&renamed));
    // An empty window still fingerprints, so a record never has to choose
    // between a fingerprint and a window that held nothing.
    assert_eq!(context_fingerprint(&snapshot(Vec::new())).len(), 16);
}

/// Every field of an entry participates in its digest: a change anywhere
/// must change the hash.
#[test]
fn entry_digest_covers_every_field() {
    let base = entry("text", 1);
    let variants = [
        entry("other", 1),
        entry("text", 2),
        RegionEntrySnapshot {
            key: Some("k".to_string()),
            ..entry("text", 1)
        },
        RegionEntrySnapshot {
            metadata: Some(serde_json::json!({"a": 1})),
            ..entry("text", 1)
        },
        RegionEntrySnapshot {
            kind: leviath_core::region::EntryKind::ToolResult {
                tool_call_id: "c1".to_string(),
                tool_name: "shell".to_string(),
                is_error: false,
            },
            ..entry("text", 1)
        },
    ];
    let base_hash = entry_digest(&base);
    let changed = variants
        .iter()
        .filter(|v| entry_digest(v) != base_hash)
        .count();
    assert_eq!(changed, variants.len(), "every field moves the digest");
    assert_eq!(entry_digest(&base), entry_digest(&entry("text", 1)));
}

/// Each kind has to survive the wire under its own name: the label is what
/// a consumer groups a token chart by, so a rename that silently reordered
/// the enum would re-attribute somebody's spend.
#[test]
fn every_inference_kind_has_a_distinct_label_and_serialized_name() {
    let all = [
        (InferenceKind::Stage, "stage"),
        (InferenceKind::Compaction, "compaction"),
        (InferenceKind::Title, "title"),
        (InferenceKind::Routing, "routing"),
    ];
    for (kind, label) in all {
        assert_eq!(kind.label(), label);
        assert_eq!(serde_json::to_value(kind).unwrap(), label);
    }
    let labels: std::collections::HashSet<_> = all.iter().map(|(k, _)| k.label()).collect();
    assert_eq!(labels.len(), all.len(), "labels must not collide");
}

/// Only stage turns are work the agent asked for. The other three are
/// machinery the runtime ran on its behalf.
#[test]
fn only_a_stage_turn_counts_as_stage_work() {
    assert!(InferenceKind::Stage.is_stage_work());
    let machinery = [
        InferenceKind::Compaction,
        InferenceKind::Title,
        InferenceKind::Routing,
    ];
    assert_eq!(machinery.iter().filter(|k| k.is_stage_work()).count(), 0);
}

/// An attempt that answered records how the answer ended, and one that did
/// not reads back with nothing there. The two keys are left off the wire when
/// empty.
#[test]
fn an_attempts_finish_reason_is_kept_and_left_off_when_empty() {
    let record = |finish_reason: &str, stopped_for: Option<&str>| AttemptRecord {
        id: "a1".to_string(),
        stage: "plan".to_string(),
        attempt: 1,
        provider: "anthropic".to_string(),
        model: "claude".to_string(),
        outcome: AttemptOutcome::Succeeded,
        finish_reason: finish_reason.to_string(),
        stopped_for: stopped_for.map(str::to_string),
        duration_ms: 10,
        backoff_ms: 0,
        digest: RequestDigest {
            system_hash: 1,
            messages: 1,
            tools: 0,
            max_tokens: 10,
            temperature: 0.0,
        },
        model_input: None,
        at: 1,
    };

    let unknown = record("unknown", Some("content_filter"));
    let json = serde_json::to_string(&unknown).unwrap();
    assert!(json.contains("\"finish_reason\":\"unknown\""));
    assert!(json.contains("\"stopped_for\":\"content_filter\""));
    let back: AttemptRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, unknown);

    let plain = record("", None);
    let json = serde_json::to_string(&plain).unwrap();
    assert!(!json.contains("finish_reason"));
    assert!(!json.contains("stopped_for"));

    let bare: AttemptRecord = serde_json::from_str(
        r#"{"stage":"plan","attempt":1,"provider":"anthropic","model":"claude",
            "outcome":"succeeded","duration_ms":10,"backoff_ms":0,
            "digest":{"system_hash":1,"messages":1,"tools":0,"max_tokens":10,"temperature":0.0},
            "at":1}"#,
    )
    .unwrap();
    assert_eq!(bare.finish_reason, "");
    assert_eq!(bare.stopped_for, None);
}

/// Every way an attempt can end, and every way the loop can follow a
/// failure, under the name a reader outside this build sees.
#[test]
fn an_attempts_outcomes_and_follow_ups_keep_their_wire_names() {
    for (outcome, json) in [
        (AttemptOutcome::Succeeded, "\"succeeded\"".to_string()),
        (
            AttemptOutcome::Failed {
                kind: "timeout".to_string(),
                transient: true,
                capacity: false,
                next: Retry::Reported,
            },
            "{\"failed\":{\"kind\":\"timeout\",\"transient\":true,\"capacity\":false,\
             \"next\":\"reported\"}}"
                .to_string(),
        ),
    ] {
        let wire = serde_json::to_string(&outcome).expect("an outcome serializes");
        assert_eq!(wire, json);
        assert_eq!(
            serde_json::from_str::<AttemptOutcome>(&wire).expect("and reads back"),
            outcome
        );
    }
    for (next, json) in [
        (Retry::Reported, "\"reported\""),
        (Retry::SameModel, "\"same_model\""),
        (Retry::RenewedFiles, "\"renewed_files\""),
    ] {
        assert_eq!(serde_json::to_string(&next).expect("serializes"), json);
        assert_eq!(
            serde_json::from_str::<Retry>(json).expect("and reads back"),
            next
        );
    }
}
