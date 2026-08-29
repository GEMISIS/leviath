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
pub(crate) struct CallUsage<'a> {
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
    /// This model's published rates, when the provider knows them.
    ///
    /// Only consulted when the provider did not report the call's cost itself
    /// (`usage.reported_cost_usd`), which is always the better figure. `None`
    /// with no reported cost makes the call unpriced, so the run reports its
    /// cost as unknown rather than as a total quietly missing this call.
    pub pricing: Option<leviath_providers::ModelPricing>,
}

/// Fold one call into the run's cumulative totals and the stage ledger, and
/// journal what it cost.
///
/// All three halves are optional and independent: a world with no `TokenTotals`
/// (a bare test agent) still journals, one with no ledger still counts, and one
/// with no persistence lane or run metadata - tests, unpersisted agents - still
/// does both. None of those absences is an error, which is why this takes
/// options rather than making callers branch.
///
/// The ledger is found by stage *name*, not by a cursor index. Three of the four
/// lanes that bill a run reach here from somewhere the cursor is awkward to
/// hold, and a stage name is the key the ledger is built on anyway. The title
/// lane has no stage, passes an empty one, and so matches no record - which is
/// correct: it is billed to the run, not to any stage of it.
pub(crate) fn record_call(
    totals: Option<&mut TokenTotals>,
    ledger: Option<&mut crate::pipeline::StageLedger>,
    persist: Option<&PersistenceStage>,
    metadata: Option<&RunMetadata>,
    call: &CallUsage<'_>,
) {
    // The provider's own figure when it gave one, else this model's rates
    // applied to the counts, else nothing at all. Decided once here so the
    // journal record, the run totals and the stage ledger cannot end up
    // describing the same call three different ways.
    let cost_usd = call.usage.priced_cost(call.pricing.as_ref());
    let cost_reported = call.usage.reported_cost_usd.is_some();
    let at = chrono::Utc::now().timestamp();
    if let Some(totals) = totals {
        totals.add_usage_priced(call.usage, call.pricing.as_ref());
    }
    if let Some(rec) = ledger.and_then(|l| l.0.iter_mut().find(|r| r.name == call.stage)) {
        rec.record_call(
            &leviath_core::run_meta::StageCall {
                prompt_tokens: call.usage.prompt_tokens,
                completion_tokens: call.usage.completion_tokens,
                cached_tokens: call.usage.cached_tokens,
                cache_write_tokens: call.usage.cache_write_tokens,
                cost_usd,
                cost_reported,
            },
            at,
        );
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
        cost_usd,
        cost_reported_by_provider: cost_reported
            .then_some(true)
            .or_else(|| call.pricing.as_ref().map(|_| false)),
        at,
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
            reported_cost_usd: None,
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
            title_error: None,
            unattended: false,
            read_paths: None,
            output_request: None,
            model_override: None,
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
            pricing: None,
        }
    }

    /// Every `Append` the lane received, in order.
    ///
    /// Drained with an `if let` rather than destructured with a `let ... else
    /// { panic!() }`: the panicking arm is a branch no test can take, and the
    /// 100% gate counts it.
    fn appended(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::persistence_bridge::PersistMsg>,
    ) -> Vec<RunRecord> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let crate::persistence_bridge::PersistMsg::Append { record, .. } = msg {
                out.push(*record);
            }
        }
        out
    }

    /// A provider that reports its own cost has that figure journaled verbatim
    /// and flagged as reported, so a later reader can tell the invoice from a
    /// reconstruction of it.
    #[test]
    fn a_reported_cost_is_journaled_as_reported() {
        let u = leviath_providers::TokenUsage::new(100, 0, 0, 20).with_reported_cost(Some(0.0042));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let noise = tx.clone();
        let mut totals = TokenTotals::default();
        record_call(
            Some(&mut totals),
            None,
            Some(&PersistenceStage(tx)),
            Some(&metadata()),
            &CallUsage {
                kind: InferenceKind::Stage,
                stage: "plan",
                iteration: 1,
                provider: "openrouter",
                model: "x-ai/grok-4.6",
                usage: &u,
                // Rates are present AND ignored: the reported figure wins.
                pricing: Some(leviath_providers::ModelPricing::flat(999.0, 999.0)),
            },
        );
        assert_eq!(totals.cost.total_usd(), Some(0.0042));
        assert!(totals.cost.is_exact());

        // The lane carries snapshots and buffered log lines on the same wire,
        // so the drain has to skip what is not an append rather than assume
        // every message is one.
        let _ = noise.send(crate::persistence_bridge::PersistMsg::StageLines {
            run_id: "run-u".to_string(),
            output_appends: vec![],
            log_appends: vec![],
        });
        let records = appended(&mut rx);
        assert_eq!(records.len(), 1, "one call, one record");
        let value = serde_json::to_value(&records[0]).unwrap();
        let f = &value["InferenceUsage"];
        assert_eq!(f["cost_usd"], serde_json::json!(0.0042));
        assert_eq!(f["cost_reported_by_provider"], serde_json::json!(true));
    }

    /// With no reported cost, the model's rates are applied and the record says
    /// so, because that number is this process's arithmetic rather than a bill.
    #[test]
    fn a_computed_cost_is_journaled_as_computed() {
        let u = leviath_providers::TokenUsage::new(1_000_000, 0, 0, 1_000_000);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut totals = TokenTotals::default();
        record_call(
            Some(&mut totals),
            None,
            Some(&PersistenceStage(tx)),
            Some(&metadata()),
            &CallUsage {
                kind: InferenceKind::Stage,
                stage: "plan",
                iteration: 1,
                provider: "anthropic",
                model: "claude-opus-5",
                usage: &u,
                pricing: Some(leviath_providers::ModelPricing::flat(5.0, 25.0)),
            },
        );
        assert_eq!(totals.cost.total_usd(), Some(30.0));
        assert!(!totals.cost.is_exact(), "arithmetic, not an invoice");

        let records = appended(&mut rx);
        assert_eq!(records.len(), 1, "one call, one record");
        let value = serde_json::to_value(&records[0]).unwrap();
        let f = &value["InferenceUsage"];
        assert_eq!(f["cost_usd"], serde_json::json!(30.0));
        assert_eq!(f["cost_reported_by_provider"], serde_json::json!(false));
    }

    /// Neither route available: the call is journaled with no cost at all
    /// rather than with a zero, which would read as "this was free".
    #[test]
    fn an_unpriced_call_journals_no_cost_rather_than_zero() {
        let u = leviath_providers::TokenUsage::new(10, 0, 0, 5);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut totals = TokenTotals::default();
        record_call(
            Some(&mut totals),
            None,
            Some(&PersistenceStage(tx)),
            Some(&metadata()),
            &call(InferenceKind::Stage, &u),
        );
        assert_eq!(totals.cost.total_usd(), None, "unknown, not zero");
        assert_eq!(totals.cost.unpriced_calls, 1);

        let records = appended(&mut rx);
        assert_eq!(records.len(), 1, "one call, one record");
        let value = serde_json::to_value(&records[0]).unwrap();
        let f = value["InferenceUsage"].as_object().unwrap();
        assert!(!f.contains_key("cost_usd"), "absent, not 0.0");
        assert!(!f.contains_key("cost_reported_by_provider"));
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
            None,
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
            None,
            Some(&PersistenceStage(tx)),
            None,
            &call(InferenceKind::Title, &u),
        );
        assert_eq!(totals.prompt_tokens, 100);
        assert!(rx.try_recv().is_err());

        // Neither: a no-op that must not panic.
        record_call(None, None, None, None, &call(InferenceKind::Routing, &u));
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
