//! What one provider call cost, recorded where it lands.
//!
//! A run bills for four kinds of call: stage turns, compaction, the title and
//! routing. Each lane's channel carries only the payload its collector wants (a
//! summary, a title, a stage name), so the usage is accounted here, in one
//! place, rather than per lane.
//!
//! Every call that was sent is recorded, however it ended: one that answered
//! by the lane that reads the answer, and one that failed or was cut off by
//! [`crate::open_calls`], which learns what it cost first. So the cumulative
//! [`TokenTotals`] cover every call, and each call sends a
//! [`RunRecord::InferenceUsage`] as it lands, which cumulative counters cannot
//! do: two calls between two steps would arrive as their sum, and a chart of
//! that sum shows a spike no single call ever made.

use leviath_providers::CallCost;

use crate::runfile::record::{InferenceKind, RunRecord};
use crate::state::journal::CallEnding;

use crate::persistence::{RunMetadata, TokenTotals};
use crate::pipeline::JournalSender;

/// Everything one call needs to report itself.
///
/// Passed as a struct rather than a dozen arguments because the four token
/// counts are trivially transposable at a call site and the compiler would not
/// catch it.
pub(crate) struct CallUsage<'a> {
    /// Which kind of call this was.
    pub kind: InferenceKind,
    /// The stage the run was in; empty for the title call, which has none.
    pub stage: &'a str,
    /// The visit of that stage the call was made in, by its id; empty where
    /// none is known, which means the visit that is open.
    pub visit: &'a str,
    /// The stage-local iteration index.
    pub iteration: usize,
    /// The provider that served the call.
    pub provider: &'a str,
    /// The model the call targeted.
    pub model: &'a str,
    /// What the call used, as far as anything said.
    pub usage: &'a leviath_providers::TokenUsage,
    /// What the call cost and how that is known: the provider's own figure,
    /// one computed from the model's rates, or no figure and why. Decided
    /// before it reaches here, so the journal record, the run totals and the
    /// stage ledger cannot describe the same call three ways.
    pub cost: CallCost,
    /// How the call ended.
    pub ending: CallEnding,
    /// The attempt this is the cost of, as the trip that made the call
    /// minted it. Empty where no trip named one, and the attempt's own record
    /// is paired with this one when both have landed.
    pub attempt: &'a str,
    /// Why the model stopped, for a call that answered, where the trip that
    /// made it said.
    pub finish_reason: Option<&'a str>,
    /// The provider's own id for the call, when it gave one.
    pub provider_call_id: Option<&'a str>,
}

/// Fold one call into the run's cumulative totals and the stage ledger, and
/// journal what it cost.
///
/// All three halves are optional and independent: a world with no `TokenTotals`
/// (a bare test agent) still journals, one with no ledger still counts, and one
/// with no journal or run metadata - tests, unpersisted agents - still
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
    persist: Option<&JournalSender>,
    metadata: Option<&RunMetadata>,
    call: &CallUsage<'_>,
) {
    let at = chrono::Utc::now().timestamp();
    if let Some(totals) = totals {
        totals.add_call(call.usage, call.cost);
    }
    if let Some(rec) = ledger.and_then(|l| l.0.iter_mut().find(|r| r.name == call.stage)) {
        rec.record_call_in(
            &leviath_core::run_meta::StageCall {
                prompt_tokens: call.usage.prompt_tokens,
                completion_tokens: call.usage.completion_tokens,
                cached_tokens: call.usage.cached_tokens,
                cache_write_tokens: call.usage.cache_write_tokens,
                cost: call.cost,
            },
            call.visit,
            at,
        );
        // Beside the money, and for the same reason: this is where a call that
        // actually happened lands. A stage records its model where it is
        // billed rather than where it is resolved, so a failover that moved
        // the stage to a second provider shows as two entries and a resolution
        // nothing ever reached shows as none.
        rec.record_model(call.provider, call.model);
    }
    let (Some(persist), Some(md)) = (persist, metadata) else {
        return;
    };
    // No ack: a usage record is telemetry, and nothing downstream waits on it
    // the way the tool lane waits on its batch record being durable before
    // anything can run.
    persist.record(&md.run_id, usage_record(call, at));
}

/// The record of what `call` used and cost, stamped `at`.
pub(crate) fn usage_record(call: &CallUsage<'_>, at: i64) -> RunRecord {
    RunRecord::InferenceUsage {
        kind: call.kind,
        stage: call.stage.to_string(),
        iteration: call.iteration,
        provider: call.provider.to_string(),
        model: call.model.to_string(),
        prompt_tokens: call.usage.prompt_tokens,
        completion_tokens: call.usage.completion_tokens,
        cached_tokens: call.usage.cached_tokens,
        cache_write_tokens: call.usage.cache_write_tokens,
        cost: call.cost,
        ending: call.ending,
        attempt: call.attempt.to_string(),
        finish_reason: call.finish_reason.map(str::to_string),
        provider_call_id: call.provider_call_id.map(str::to_string),
        at,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use leviath_providers::{CostUnknown, ModelPricing, TokenUsage};

    fn usage() -> TokenUsage {
        let mut u = TokenUsage::new(100, 3, 4, 20);
        u.total_tokens = 120;
        u
    }

    pub(crate) fn metadata() -> RunMetadata {
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
            title: None,
            title_error: None,
            blueprint_digest: None,
            unattended: leviath_core::Unattended::Off,
            read_paths: None,
            output_request: None,
            model_override: None,
        }
    }

    /// A call that answered with `u`, priced at `pricing`.
    fn call<'a>(
        kind: InferenceKind,
        u: &'a TokenUsage,
        pricing: Option<ModelPricing>,
    ) -> CallUsage<'a> {
        CallUsage {
            kind,
            stage: "plan",
            visit: "",
            iteration: 2,
            provider: "anthropic",
            model: "claude-sonnet-5",
            usage: u,
            cost: u.cost(pricing.as_ref()),
            ending: CallEnding::Answered,
            attempt: "",
            provider_call_id: None,
            finish_reason: None,
        }
    }

    /// Every record the journal received, in order.
    fn appended(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::pipeline::journal::Journaled>,
    ) -> Vec<RunRecord> {
        let mut out = Vec::new();
        while let Ok(sent) = rx.try_recv() {
            out.push(*sent.record);
        }
        out
    }

    /// One call recorded into fresh totals, with what the journal got.
    fn recorded(call: &CallUsage<'_>) -> (TokenTotals, Vec<RunRecord>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut totals = TokenTotals::default();
        record_call(
            Some(&mut totals),
            None,
            Some(&JournalSender::new(tx, None)),
            Some(&metadata()),
            call,
        );
        (totals, appended(&mut rx))
    }

    /// A provider that reports its own cost has that figure journaled verbatim
    /// and counted as reported, whatever rates are at hand.
    #[test]
    fn a_reported_cost_is_journaled_as_reported() {
        let u = TokenUsage::new(100, 0, 0, 20).with_reported_cost(Some(0.0042));
        // Rates are present AND ignored: the reported figure wins.
        let (totals, records) = recorded(&call(
            InferenceKind::Stage,
            &u,
            Some(ModelPricing::flat(999.0, 999.0)),
        ));
        assert_eq!(totals.cost.total_usd(), Some(0.0042));
        assert_eq!(totals.cost.reported_calls, 1);
        assert_eq!(records.len(), 1, "one call, one record");
        assert!(format!("{:?}", records[0]).contains("cost: Reported(0.0042)"));
    }

    /// With no reported cost, the model's rates are applied and the record
    /// says so: the figure is this process's arithmetic, never labelled as
    /// the provider's.
    #[test]
    fn a_computed_cost_is_journaled_as_computed() {
        let u = TokenUsage::new(1_000_000, 0, 0, 1_000_000);
        let (totals, records) = recorded(&call(
            InferenceKind::Stage,
            &u,
            Some(ModelPricing::flat(5.0, 25.0)),
        ));
        assert_eq!(totals.cost.total_usd(), Some(30.0));
        assert_eq!(
            (totals.cost.reported_calls, totals.cost.computed_calls),
            (0, 1)
        );
        assert!(totals.cost.is_exact(), "a computed cost is a known cost");
        assert!(format!("{:?}", records[0]).contains("cost: Computed(30.0)"));
    }

    /// Neither route available: the call is journaled with no figure and the
    /// reason for it, rather than with a zero, which would read as "this was
    /// free".
    #[test]
    fn an_unknown_cost_is_journaled_with_its_reason_rather_than_zero() {
        let u = TokenUsage::new(10, 0, 0, 5);
        let (totals, records) = recorded(&call(InferenceKind::Stage, &u, None));
        assert_eq!(totals.cost.total_usd(), None, "unknown, not zero");
        assert_eq!(totals.cost.unknown.no_price, 1);
        assert!(format!("{:?}", records[0]).contains("cost: Unknown(NoPrice)"));
    }

    /// A call that did not answer is recorded like any other: its tokens as
    /// far as anything said, how it ended, the attempt it was and the
    /// provider's own id for it.
    #[test]
    fn a_call_that_did_not_answer_is_recorded_with_how_it_ended() {
        let u = TokenUsage::new(7, 0, 0, 0);
        let (totals, records) = recorded(&CallUsage {
            cost: leviath_providers::CallCost::Unknown(CostUnknown::NoLookup),
            ending: CallEnding::CutOff,
            attempt: "att-9",
            provider_call_id: Some("gen-9"),
            ..call(InferenceKind::Stage, &u, None)
        });
        assert_eq!(totals.prompt_tokens, 7);
        assert_eq!(totals.cost.unknown.no_lookup, 1);
        let shown = format!("{:?}", records[0]);
        assert!(
            shown.contains("cost: Unknown(NoLookup), ending: CutOff"),
            "{shown}"
        );
        assert!(
            shown.contains(
                "attempt: \"att-9\", finish_reason: None, provider_call_id: Some(\"gen-9\")"
            ),
            "{shown}"
        );
    }

    /// The halves are independent by design, so the combinations of "has
    /// totals" and "has a journal" all have to behave. A world missing either
    /// is ordinary - tests and unpersisted agents run that way - and an
    /// absence must not cost the half that is present.
    #[test]
    fn counting_and_journaling_are_independent() {
        let u = usage();

        // Both present: counted and written.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut totals = TokenTotals::default();
        record_call(
            Some(&mut totals),
            None,
            Some(&JournalSender::new(tx, None)),
            Some(&metadata()),
            &call(InferenceKind::Compaction, &u, None),
        );
        assert_eq!(totals.prompt_tokens, 100);
        let mut appended: Vec<(String, RunRecord)> = Vec::new();
        while let Ok(sent) = rx.try_recv() {
            appended.push((sent.run_id, *sent.record));
        }
        assert_eq!(appended.len(), 1, "one call, one record");
        let (run_id, record) = appended.remove(0);
        assert_eq!(run_id, "run-u");
        // Every field but the wall-clock stamp, the one value that cannot be
        // known ahead of time.
        let shown = format!("{record:?}");
        assert!(
            shown.starts_with(
                "InferenceUsage { kind: Compaction, stage: \"plan\", iteration: 2, \
                 provider: \"anthropic\", model: \"claude-sonnet-5\", prompt_tokens: 100, \
                 completion_tokens: 20, cached_tokens: 3, cache_write_tokens: 4, \
                 cost: Unknown(NoPrice), ending: Answered, attempt: \"\", \
                 finish_reason: None, provider_call_id: None, at: "
            ),
            "{shown}"
        );

        // No journal: still counted.
        let mut totals = TokenTotals::default();
        record_call(
            Some(&mut totals),
            None,
            None,
            Some(&metadata()),
            &call(InferenceKind::Stage, &u, None),
        );
        assert_eq!(totals.prompt_tokens, 100);

        // A journal but no run metadata: nothing to address the record to, so
        // nothing is written - and the count still lands.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut totals = TokenTotals::default();
        record_call(
            Some(&mut totals),
            None,
            Some(&JournalSender::new(tx, None)),
            None,
            &call(InferenceKind::Title, &u, None),
        );
        assert_eq!(totals.prompt_tokens, 100);
        assert!(rx.try_recv().is_err());

        // Neither: a no-op that must not panic.
        record_call(
            None,
            None,
            None,
            None,
            &call(InferenceKind::Routing, &u, None),
        );
    }

    /// The stage the call was billed to records what served it, and a stage
    /// that moved between providers keeps both in the order it reached them.
    ///
    /// The title call names no stage, so it matches no record and leaves the
    /// ledger alone: it is billed to the run rather than to any stage of it.
    #[test]
    fn a_stage_records_every_provider_and_model_that_served_it() {
        use leviath_core::run_meta::{StageModelUse, StageRecord};

        let u = usage();
        let mut ledger = crate::pipeline::StageLedger(vec![
            StageRecord::new("plan".to_string(), 0),
            StageRecord::new("code".to_string(), 1),
        ]);
        for (stage, provider, model) in [
            ("plan", "anthropic", "claude-opus-5"),
            ("plan", "anthropic", "claude-opus-5"),
            ("plan", "openrouter", "anthropic/claude-opus-5"),
            ("", "openai", "gpt-5-mini"),
        ] {
            record_call(
                None,
                Some(&mut ledger),
                None,
                None,
                &CallUsage {
                    stage,
                    provider,
                    model,
                    iteration: 1,
                    ..call(InferenceKind::Stage, &u, None)
                },
            );
        }
        assert_eq!(
            ledger.0[0].models,
            vec![
                StageModelUse {
                    provider: "anthropic".to_string(),
                    model: "claude-opus-5".to_string(),
                },
                StageModelUse {
                    provider: "openrouter".to_string(),
                    model: "anthropic/claude-opus-5".to_string(),
                },
            ],
        );
        assert_eq!(ledger.0[0].unknown_calls, 3, "each call, with no price");
        assert!(
            ledger.0[1].models.is_empty(),
            "a stage the run has not run in has nothing to report"
        );
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
                &call(InferenceKind::Stage, &u, None),
            );
        }
        assert_eq!(totals.prompt_tokens, 300);
        assert_eq!(totals.completion_tokens, 60);
        assert_eq!(totals.cached_tokens, 9);
        assert_eq!(totals.cache_write_tokens, 12);
    }
}
