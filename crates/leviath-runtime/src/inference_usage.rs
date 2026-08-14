//! What one provider call cost, recorded where it lands.
//!
//! A run bills for four kinds of call and, until this module existed, counted
//! one of them. Stage turns went into [`TokenTotals`];
//! the compaction, title, and routing lanes each threw their usage away at the
//! outcome boundary, because each channel carried only the payload its collector
//! wanted - a summary, a title, a stage name - and usage was not it.
//!
//! Two things follow from putting the accounting in one place rather than
//! repeating it per lane. The cumulative totals finally cover every call, so a
//! run's reported spend is what the provider actually billed. And each call
//! writes a [`RunRecord::InferenceUsage`] as it lands, which is the part
//! [`RunRecord::Progress`] cannot do: progress counters are cumulative, so two
//! calls between two ticks arrive as their sum, and a chart of that sum shows a
//! spike no single call ever made.

use leviath_core::run_archive::{InferenceKind, RunRecord};

use crate::persistence::{RunMetadata, TokenTotals};
use crate::persistence_bridge::PersistMsg;
use crate::pipeline::PersistenceStage;

/// Everything one call needs to report itself.
///
/// Passed as a struct rather than eight arguments because the four token counts
/// are trivially transposable at a call site and the compiler would not catch
/// it.
pub struct CallUsage<'a> {
    /// Which kind of call this was.
    pub kind: InferenceKind,
    /// The stage the run was in; empty for the title call, which has none.
    pub stage: &'a str,
    /// The stage-local iteration index.
    pub iteration: usize,
    /// The provider that served the call.
    pub provider: &'a str,
    /// The model the call targeted.
    pub model: &'a str,
    /// What the provider billed.
    pub usage: &'a leviath_providers::TokenUsage,
}

/// Fold one call into the run's cumulative totals and journal what it cost.
///
/// Both halves are optional and independent: a world with no `TokenTotals` (a
/// bare test agent) still journals, and a world with no persistence lane or run
/// metadata - tests, unpersisted agents - still counts. Neither absence is an
/// error, which is why this takes options rather than making callers branch.
pub fn record_call(
    totals: Option<&mut TokenTotals>,
    persist: Option<&PersistenceStage>,
    metadata: Option<&RunMetadata>,
    call: &CallUsage<'_>,
) {
    if let Some(totals) = totals {
        totals.add_usage(call.usage);
    }
    let (Some(persist), Some(md)) = (persist, metadata) else {
        return;
    };
    let record = RunRecord::InferenceUsage {
        kind: call.kind,
        stage: call.stage.to_string(),
        iteration: call.iteration,
        provider: call.provider.to_string(),
        model: call.model.to_string(),
        prompt_tokens: call.usage.prompt_tokens,
        completion_tokens: call.usage.completion_tokens,
        cached_tokens: call.usage.cached_tokens,
        cache_write_tokens: call.usage.cache_write_tokens,
        at: chrono::Utc::now().timestamp(),
    };
    // No ack: a usage record is telemetry, and nothing downstream waits on it
    // the way the tool lane waits on its batch record being durable before
    // anything can run.
    let _ = persist.0.send(PersistMsg::Append {
        run_id: md.run_id.clone(),
        record: Box::new(record),
        ack: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage() -> leviath_providers::TokenUsage {
        leviath_providers::TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 20,
            cached_tokens: 3,
            cache_write_tokens: 4,
            total_tokens: 120,
        }
    }

    fn metadata() -> RunMetadata {
        RunMetadata {
            run_id: "run-u".to_string(),
            agent_name: "a".to_string(),
            agent_path: "/a".to_string(),
            task: "t".to_string(),
            model: None,
            workdir: "/w".to_string(),
            num_stages: 1,
            started_at: 0,
            parent_run_id: None,
            metadata: Default::default(),
            callback_url: None,
            callback_secret: None,
            title: None,
            unattended: false,
            read_paths: None,
            output_request: None,
        }
    }

    fn call(kind: InferenceKind, u: &leviath_providers::TokenUsage) -> CallUsage<'_> {
        CallUsage {
            kind,
            stage: "plan",
            iteration: 2,
            provider: "anthropic",
            model: "claude-sonnet-5",
            usage: u,
        }
    }

    /// The two halves are independent by design, so the four combinations of
    /// "has totals" and "has a journal" all have to behave. A world missing
    /// either is ordinary - tests and unpersisted agents run that way - and an
    /// absence must not cost the half that is present.
    #[test]
    fn counting_and_journaling_are_independent() {
        let u = usage();

        // Both present: counted and written.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx_for_noise = tx.clone();
        let mut totals = TokenTotals::default();
        record_call(
            Some(&mut totals),
            Some(&PersistenceStage(tx)),
            Some(&metadata()),
            &call(InferenceKind::Compaction, &u),
        );
        assert_eq!(totals.prompt_tokens, 100);
        // The persistence channel carries snapshots and buffered log lines on
        // the same wire, so the drain has to pick ours out of mixed traffic
        // rather than assume the next message is it.
        let _ = tx_for_noise.send(crate::persistence_bridge::PersistMsg::StageLines {
            run_id: "run-u".to_string(),
            output_appends: vec![],
            log_appends: vec![],
        });
        let mut appended: Vec<(String, RunRecord)> = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let crate::persistence_bridge::PersistMsg::Append { run_id, record, .. } = msg {
                appended.push((run_id, *record));
            }
        }
        assert_eq!(appended.len(), 1, "one call, one record");
        let (run_id, record) = appended.remove(0);
        assert_eq!(run_id, "run-u");
        // Asserted on the serialized form with the wall-clock stamp lifted out,
        // so this pins the field names a journal reader parses without pinning
        // the one value that cannot be known ahead of time.
        let mut value = serde_json::to_value(&record).unwrap();
        let fields = value["InferenceUsage"].as_object_mut().unwrap();
        assert!(fields.remove("at").is_some(), "a call is stamped");
        assert_eq!(
            value,
            serde_json::json!({
                "InferenceUsage": {
                    "kind": "compaction",
                    "stage": "plan",
                    "iteration": 2,
                    "provider": "anthropic",
                    "model": "claude-sonnet-5",
                    "prompt_tokens": 100,
                    "completion_tokens": 20,
                    "cached_tokens": 3,
                    "cache_write_tokens": 4,
                }
            })
        );

        // No journal: still counted.
        let mut totals = TokenTotals::default();
        record_call(
            Some(&mut totals),
            None,
            Some(&metadata()),
            &call(InferenceKind::Stage, &u),
        );
        assert_eq!(totals.prompt_tokens, 100);

        // A journal but no run metadata: nothing to address the record to, so
        // nothing is written - and the count still lands.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut totals = TokenTotals::default();
        record_call(
            Some(&mut totals),
            Some(&PersistenceStage(tx)),
            None,
            &call(InferenceKind::Title, &u),
        );
        assert_eq!(totals.prompt_tokens, 100);
        assert!(rx.try_recv().is_err());

        // Neither: a no-op that must not panic.
        record_call(None, None, None, &call(InferenceKind::Routing, &u));
    }

    /// Totals accumulate across calls rather than being overwritten - the bug
    /// that made a run report its last call instead of its bill would pass a
    /// single-call test.
    #[test]
    fn repeated_calls_accumulate() {
        let u = usage();
        let mut totals = TokenTotals::default();
        for _ in 0..3 {
            record_call(
                Some(&mut totals),
                None,
                None,
                &call(InferenceKind::Stage, &u),
            );
        }
        assert_eq!(totals.prompt_tokens, 300);
        assert_eq!(totals.completion_tokens, 60);
        assert_eq!(totals.cached_tokens, 9);
        assert_eq!(totals.cache_write_tokens, 12);
    }
}
