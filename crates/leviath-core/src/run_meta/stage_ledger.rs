//! The per-stage ledger: where a run's tokens and money went, stage by stage
//! and stay by stay.
//!
//! Split out of [`run_meta`](super) rather than kept beside [`RunMeta`] because
//! it answers a different question. `RunMeta` is what a run *is* - where it is,
//! what it holds, whether it is still going. This is what it *spent*, and the
//! rules that govern spending are its own: an unknown cost must never render as
//! a number, exactness only ever decays, and a stage entered twice is two stays
//! rather than one sum.
//!
//! Written to `stages.json` beside `meta.json`, rewritten whole on every persist
//! tick, and served verbatim by `GET /api/agents/{id}/stages`.

use serde::{Deserialize, Serialize};

use super::ActiveClock;
#[cfg(doc)]
use super::RunMeta;

/// Status of an individual stage within a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StageRunStatus {
    /// Declared but not yet entered.
    Pending,
    /// The stage the run is in right now. At most one stage is `Active`.
    Active,
    /// Entered, and blocked on a person answering.
    WaitingInput,
    /// Finished and left. A stage that loops back becomes `Active` again.
    Complete,
    /// Ended in a failure. The run's own `error` carries the message.
    Error,
    /// The run finished without ever entering this stage.
    ///
    /// Distinct from [`Pending`](Self::Pending), which means "not yet" while a
    /// run is live, and from [`Complete`](Self::Complete), which these used to
    /// be recorded as: the ledger marked every stage positioned before the
    /// cursor complete, and a graph does not visit its stages in index order,
    /// so an error-recovery branch nothing reached was filed as having run
    /// (#372). Its `region_tokens` is empty because nothing ever wrote it,
    /// which made the next real stage look like it had written every region
    /// from zero.
    Skipped,
}

impl std::fmt::Display for StageRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StageRunStatus::Pending => write!(f, "Pending"),
            StageRunStatus::Skipped => write!(f, "Skipped"),
            StageRunStatus::Active => write!(f, "Active"),
            StageRunStatus::WaitingInput => write!(f, "WaitingInput"),
            StageRunStatus::Complete => write!(f, "Complete"),
            StageRunStatus::Error => write!(f, "Error"),
        }
    }
}

/// One provider call, in the shape the stage ledger has to fold it in.
///
/// A struct rather than six arguments because the four token counts are
/// trivially transposable at a call site and the compiler would not catch it -
/// the same reason the runtime's own `CallUsage` is one.
///
/// Pricing is already resolved by the time a call reaches here. Choosing
/// between the provider's own figure and a rate card is the runtime's job and
/// is done in exactly one place; this type carries that answer, not the rule
/// that produced it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StageCall {
    /// Fresh input tokens, exclusive of both cache counts.
    pub prompt_tokens: usize,
    /// Output tokens.
    pub completion_tokens: usize,
    /// Input tokens served from the provider's cache.
    pub cached_tokens: usize,
    /// Input tokens written into the provider's cache.
    pub cache_write_tokens: usize,
    /// What this call cost, or `None` when neither the provider nor a rate card
    /// could price it.
    ///
    /// Never a zero standing in for unknown: a zero is a claim that the call was
    /// free, which is true of local inference and false of everything else.
    pub cost_usd: Option<f64>,
    /// Whether [`cost_usd`](Self::cost_usd) is the provider's own figure rather
    /// than one reconstructed from published rates.
    pub cost_reported: bool,
}

/// How many visits one [`StageRecord`] keeps in detail.
///
/// `stages.json` is rewritten whole on every persist tick, so an unbounded list
/// is a file that grows for as long as a looping run does and is re-serialized
/// every time. A run that enters one stage more often than this has stopped
/// being a graph anyone reads node by node, and the accumulated per-stage
/// figures are the ones worth having there - which [`StageRecord`] keeps either
/// way.
///
/// The earliest visits are the ones kept, so a visit's position in the list is
/// stable: `visits[2]` means the same visit on every poll.
pub const MAX_STAGE_VISITS: usize = 128;

/// One contiguous stay in a stage: what it cost, and how long it took.
///
/// A [`StageRecord`] accumulates across revisits, which is the figure most
/// readers want and the only one a resumed run can carry. It is the wrong shape
/// for a graph of the path a run actually took, where a stage entered twice is
/// two nodes: attributing the accumulated total to the first node overstates it,
/// splitting it evenly invents a division, and a reader can see neither mistake.
/// So an entry is opened when the stage is entered, closed when it is left, and
/// every call folds into whichever visit was open at the time.
///
/// One entry per *entry into the stage*, matching the visit number the
/// `stage_transition` event carries: a stage that loops back to itself starts a
/// new visit, while iterations within one stay do not.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StageVisitRecord {
    /// Unix seconds when the run entered the stage on this visit.
    pub entered_at: i64,
    /// Unix seconds when it left. `None` on the visit in progress, which is the
    /// last entry of the stage the run is in right now.
    pub left_at: Option<i64>,
    /// Input tokens billed during this visit.
    pub prompt_tokens: usize,
    /// Output tokens billed during this visit.
    pub completion_tokens: usize,
    /// Tokens read from provider cache during this visit.
    pub cached_tokens: usize,
    /// Tokens written to provider cache during this visit.
    pub cache_write_tokens: usize,
    /// What this visit spent, when every one of its calls could be priced.
    ///
    /// `None` means unknown, never free - the same meaning it carries on
    /// [`RunMeta::cost_usd`].
    pub cost_usd: Option<f64>,
    /// Calls in this visit that could not be priced at all. Non-zero forces
    /// [`cost_usd`](Self::cost_usd) to `None`.
    pub unpriced_calls: usize,
    /// Whether every priced call in this visit carried the provider's own cost
    /// figure rather than one computed from published rates.
    pub cost_is_exact: bool,
    /// The priced subtotal, kept even while `cost_usd` is `None` so a resumed
    /// run does not restart this visit's accounting from zero.
    pub cost_priced_usd: f64,
    /// How long this visit actually spent working, as against how long the run
    /// was parked in the stage. Read it through
    /// [`active_runtime_secs`](Self::active_runtime_secs).
    #[serde(default)]
    pub active: Option<ActiveClock>,
}

impl StageVisitRecord {
    /// A visit that has just started and billed nothing.
    pub fn opened_at(at: i64) -> Self {
        Self {
            entered_at: at,
            left_at: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            // No calls is a genuine zero, and every call folded in from here can
            // only make the figure less certain. A fresh run starts the same way.
            cost_usd: Some(0.0),
            unpriced_calls: 0,
            cost_is_exact: true,
            cost_priced_usd: 0.0,
            active: None,
        }
    }

    /// Fold one call into this visit.
    pub fn record_call(&mut self, call: &StageCall) {
        self.prompt_tokens += call.prompt_tokens;
        self.completion_tokens += call.completion_tokens;
        self.cached_tokens += call.cached_tokens;
        self.cache_write_tokens += call.cache_write_tokens;
        match call.cost_usd {
            Some(usd) => self.cost_priced_usd += usd,
            None => self.unpriced_calls += 1,
        }
        // One call priced from a rate card makes the whole figure a
        // reconstruction, and nothing later can turn it back into the invoice.
        self.cost_is_exact &= call.cost_reported;
        self.cost_usd = (self.unpriced_calls == 0).then_some(self.cost_priced_usd);
    }

    /// How long this visit has actually been working, at `now`.
    ///
    /// Falls back to the wall-clock span for records written before the clock
    /// existed, for the reason given on [`RunMeta::active_runtime_secs`].
    pub fn active_runtime_secs(&self, now: i64) -> u64 {
        if let Some(clock) = self.active {
            return clock.total_secs(now);
        }
        crate::duration::between(self.entered_at, self.left_at.unwrap_or(now))
    }
}

/// Metadata record for a single stage within a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageRecord {
    /// The stage's name, matching its key under `[stages]`.
    pub name: String,
    /// Zero-based position in the blueprint's stage list.
    pub index: usize,
    /// Where this stage stands.
    pub status: StageRunStatus,
    /// Whether the run has ever actually been in this stage.
    ///
    /// Position cannot answer this. A graph blueprint reaches its stages in
    /// whatever order its edges describe, so "index below the cursor" includes
    /// every branch the run went past without taking - and reading it as
    /// "finished" is what filed never-entered stages as `Complete` (#372).
    /// Sticky once set, so a stage the run has left and may re-enter stays
    /// entered.
    #[serde(default)]
    pub entered: bool,
    /// Input tokens billed while this stage was active. A revisited stage keeps
    /// accumulating rather than resetting, so the run's total is the sum.
    pub prompt_tokens: usize,
    /// Output tokens billed while this stage was active, accumulating the same
    /// way.
    pub completion_tokens: usize,
    /// Tokens read from provider cache in this stage.
    #[serde(default)]
    pub cached_tokens: usize,
    /// Tokens *written* to provider cache in this stage.
    ///
    /// Without it only half of a cache decision was visible: a stage showing
    /// no reads might be paying to write a prefix nothing reuses, or might not
    /// be caching at all, and the ledger could not tell those apart.
    #[serde(default)]
    pub cache_write_tokens: usize,
    /// What this stage spent, when every one of its calls could be priced.
    ///
    /// `None` means *unknown*, never free, exactly as on
    /// [`RunMeta::cost_usd`]: some call was served by a model with no reported
    /// cost and no known rates, so any total would understate by an unknown
    /// amount. The tokens beside it were always here; this is the one
    /// conversion that has a rule behind it, and it belongs on the side that
    /// owns the rule. A console multiplying these tokens by a rate card of its
    /// own would produce a fourth answer that disagrees with the other three
    /// (#630).
    ///
    /// Accumulated across revisits, like the token counts. For the split by
    /// visit, read [`visits`](Self::visits).
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Calls in this stage that could not be priced at all. Non-zero forces
    /// [`cost_usd`](Self::cost_usd) to `None`.
    #[serde(default)]
    pub unpriced_calls: usize,
    /// Whether every priced call in this stage carried the provider's own cost
    /// figure rather than one computed from published rates. `false` means the
    /// total is a reconstruction of the invoice, not the invoice.
    #[serde(default)]
    pub cost_is_exact: bool,
    /// The priced subtotal, kept even when `cost_usd` is `None` so a resumed run
    /// does not restart this stage's accounting from zero.
    #[serde(default)]
    pub cost_priced_usd: f64,
    /// Each contiguous stay in this stage, oldest first.
    ///
    /// The record above accumulates across revisits, which is the right total
    /// and the wrong shape for a graph of the path a run took. These are the
    /// nodes of that graph: one entry per entry into the stage, each with the
    /// tokens and cost billed while it was open.
    ///
    /// Empty on a stage the run never entered, and on records written before
    /// Leviath split visits out - which is why the accumulated figures above
    /// stay the ones to fall back to. Capped at [`MAX_STAGE_VISITS`]; the
    /// accumulated figures keep counting past the cap.
    #[serde(default)]
    pub visits: Vec<StageVisitRecord>,
    /// How many times the run has entered this stage, counting past the point
    /// where [`visits`](Self::visits) stopped recording them.
    ///
    /// A separate count rather than `visits.len()` so a capped list cannot be
    /// read as the whole story: `visit_count > visits.len()` is the signal that
    /// the per-visit split is partial and the accumulated figures are the
    /// complete ones.
    #[serde(default)]
    pub visit_count: usize,
    /// Per-region token contribution to this stage's calls, by region name.
    ///
    /// The central question of a structured layout is "what am I paying to
    /// carry, and where", and answering it meant replaying the context history
    /// and grouping by stage - archaeology for something the runtime already
    /// knows. Recorded as the largest each region reached while the stage was
    /// active, which is the number that decides whether a region is earning its
    /// place.
    ///
    /// Every region the window carries is measured, including the ones a stage
    /// layout hides rather than declares, so a stage can list a region it never
    /// assembled into a request.
    #[serde(default)]
    pub region_tokens: std::collections::BTreeMap<String, usize>,
    /// Prompt tokens billed by this stage's first call, the baseline the
    /// runaway-context check compares against. `None` until it runs once.
    #[serde(default)]
    pub first_call_prompt_tokens: Option<usize>,
    /// Whether the runaway-context warning has already fired for this stage, so
    /// it is said once on the crossing rather than on every call afterwards.
    #[serde(default)]
    pub runaway_warned: bool,
    /// A reply in this stage was cut off at the output cap, so its requests go
    /// out at the model's maximum. Kept here, and not only in the per-stage
    /// runtime counters, because those do not survive a daemon restart and a
    /// resumed run would otherwise retry at the cap that already failed.
    #[serde(default)]
    pub output_cap_raised: bool,
    /// Unix timestamp (seconds); None until the stage starts.
    pub started_at: Option<i64>,
    /// Unix timestamp (seconds); None until the stage ends.
    pub ended_at: Option<i64>,
    /// How long this stage has actually been working, as against how long it has
    /// been the cursor. Read it through [`StageRecord::active_runtime_secs`].
    ///
    /// A stage the run is parked in - paused, or holding a prompt open - is
    /// still the cursor stage, so `started_at`..`ended_at` counts time nothing
    /// spent working. A stage the run re-enters keeps accumulating, the same way
    /// its token counts do.
    ///
    /// `None` on records written before the clock existed; see
    /// [`RunMeta::active`] for why that is not a zero.
    #[serde(default)]
    pub active: Option<ActiveClock>,
}

impl StageRecord {
    /// How long this stage has actually been working, at `now`.
    ///
    /// Falls back to the wall-clock span for records written before the clock
    /// existed, for the reason given on [`RunMeta::active_runtime_secs`].
    pub fn active_runtime_secs(&self, now: i64) -> u64 {
        if let Some(clock) = self.active {
            return clock.total_secs(now);
        }
        let Some(started) = self.started_at else {
            return 0;
        };
        crate::duration::between(started, self.ended_at.unwrap_or(now))
    }

    /// A stage the run has not entered yet: [`StageRunStatus::Pending`], zero
    /// tokens, and neither timestamp set.
    pub fn new(name: String, index: usize) -> Self {
        Self {
            name,
            index,
            status: StageRunStatus::Pending,
            entered: false,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: Some(0.0),
            unpriced_calls: 0,
            cost_is_exact: true,
            cost_priced_usd: 0.0,
            visits: Vec::new(),
            visit_count: 0,
            region_tokens: std::collections::BTreeMap::new(),
            first_call_prompt_tokens: None,
            runaway_warned: false,
            output_cap_raised: false,
            started_at: None,
            ended_at: None,
            active: None,
        }
    }

    /// Fold one provider call into this stage, and into the visit that was open
    /// when it was made.
    ///
    /// Every call a run bills is a call made *somewhere*, so a compaction or a
    /// routing call counts against the stage it happened in exactly as a stage
    /// turn does. The one exception is the title call, which has no stage at
    /// all and reaches no record here.
    ///
    /// A call arriving with no visit open opens one at `at`. That is not a
    /// normal path - entering the stage opens it - but a call is proof the run
    /// was in the stage, and dropping the money because the bookkeeping was
    /// late would be the one failure this ledger exists to prevent.
    pub fn record_call(&mut self, call: &StageCall, at: i64) {
        self.prompt_tokens += call.prompt_tokens;
        self.completion_tokens += call.completion_tokens;
        self.cached_tokens += call.cached_tokens;
        self.cache_write_tokens += call.cache_write_tokens;
        match call.cost_usd {
            Some(usd) => self.cost_priced_usd += usd,
            None => self.unpriced_calls += 1,
        }
        self.cost_is_exact &= call.cost_reported;
        self.cost_usd = (self.unpriced_calls == 0).then_some(self.cost_priced_usd);
        if let Some(visit) = self.open_visit(at) {
            visit.record_call(call);
        }
    }

    /// The visit in progress, starting one at `at` if the last has been closed
    /// or there is none.
    ///
    /// `None` once [`MAX_STAGE_VISITS`] have been recorded: the stage's own
    /// totals keep counting, and the per-visit split stops rather than growing
    /// a file that is rewritten whole on every tick.
    pub fn open_visit(&mut self, at: i64) -> Option<&mut StageVisitRecord> {
        if matches!(self.visits.last(), Some(v) if v.left_at.is_none()) {
            return self.visits.last_mut();
        }
        // Past the cap, nothing is opened and nothing is counted: counting here
        // would turn `visit_count` into a count of calls, since every call after
        // the cap finds no visit open.
        if self.visits.len() >= MAX_STAGE_VISITS {
            return None;
        }
        self.start_visit(at);
        self.visits.last_mut()
    }

    /// Start a new visit at `at`, closing whatever was open first.
    ///
    /// Called when the run enters the stage, which is the only place the
    /// boundary is exact. A self-transition is an entry like any other and
    /// starts a new visit, matching the visit number the `stage_transition`
    /// event carries.
    pub fn begin_visit(&mut self, at: i64) {
        self.close_visit(at);
        self.start_visit(at);
    }

    /// Count one entry into the stage, recording it in detail while there is
    /// room. The count runs past the cap; the list does not.
    fn start_visit(&mut self, at: i64) {
        self.visit_count += 1;
        if self.visits.len() < MAX_STAGE_VISITS {
            self.visits.push(StageVisitRecord::opened_at(at));
        }
    }

    /// Close the visit in progress at `at`, stopping its clock. Idempotent: a
    /// stage with nothing open is left alone, so the persist tick can call it
    /// on every stage the run is not in.
    pub fn close_visit(&mut self, at: i64) {
        let Some(visit) = self.visits.last_mut() else {
            return;
        };
        if visit.left_at.is_some() {
            return;
        }
        visit.left_at = Some(at);
        visit.active.get_or_insert_default().observe(at, false);
    }

    /// Run the open visit's working clock forward to `now`, matching the
    /// stage's own. A no-op when nothing is open.
    pub fn observe_visit(&mut self, now: i64, running: bool) {
        let Some(visit) = self.visits.last_mut() else {
            return;
        };
        if visit.left_at.is_some() {
            return;
        }
        visit.active.get_or_insert_default().observe(now, running);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_record_new_and_serde_roundtrip() {
        let rec = StageRecord::new("analyze".to_string(), 2);
        assert_eq!(rec.name, "analyze");
        assert_eq!(rec.index, 2);
        assert_eq!(rec.status, StageRunStatus::Pending);
        assert_eq!(rec.prompt_tokens, 0);
        assert_eq!(rec.completion_tokens, 0);
        assert_eq!(rec.cached_tokens, 0);
        assert!(rec.started_at.is_none());
        assert!(rec.ended_at.is_none());

        let json = serde_json::to_string(&rec).unwrap();
        let back: StageRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "analyze");
        assert_eq!(back.status, StageRunStatus::Pending);
    }

    /// A call priced from a rate card. `cost_reported: false` is the half that
    /// decides `cost_is_exact`, and it is not derivable from the amount.
    fn computed(cost_usd: f64) -> StageCall {
        StageCall {
            prompt_tokens: 10,
            completion_tokens: 2,
            cached_tokens: 1,
            cache_write_tokens: 3,
            cost_usd: Some(cost_usd),
            cost_reported: false,
        }
    }

    /// Costs add up across calls rather than being overwritten, and the tokens
    /// beside them describe the same set of calls.
    #[test]
    fn a_stages_cost_accumulates_with_its_tokens() {
        let mut rec = StageRecord::new("gather".to_string(), 0);
        rec.record_call(&computed(0.25), 100);
        rec.record_call(&computed(0.75), 110);
        assert_eq!(rec.cost_usd, Some(1.0));
        assert_eq!(rec.cost_priced_usd, 1.0);
        assert_eq!(rec.prompt_tokens, 20);
        assert_eq!(rec.completion_tokens, 4);
        assert_eq!(rec.cached_tokens, 2);
        assert_eq!(rec.cache_write_tokens, 6);
        assert_eq!(rec.unpriced_calls, 0);
        assert!(!rec.cost_is_exact, "rates, not an invoice");
    }

    /// One call nobody could price takes the whole stage's figure with it. The
    /// priced part is kept so a resumed run does not start over, and it is not
    /// what `cost_usd` reports: a partial total looks authoritative and
    /// understates by however much it skipped.
    #[test]
    fn one_unpriced_call_makes_the_stage_unknown_rather_than_cheap() {
        let mut rec = StageRecord::new("gather".to_string(), 0);
        rec.record_call(&computed(4.0), 100);
        assert_eq!(rec.cost_usd, Some(4.0));

        rec.record_call(
            &StageCall {
                cost_usd: None,
                ..Default::default()
            },
            110,
        );
        assert_eq!(rec.cost_usd, None, "unknown, not 4.0 and not 0.0");
        assert_eq!(rec.unpriced_calls, 1);
        assert_eq!(rec.cost_priced_usd, 4.0, "the priced part survives");
    }

    /// Exactness only ever goes one way. A stage whose first call carried the
    /// provider's own figure and whose second was reconstructed holds a
    /// reconstruction, and no later reported call turns it back into the invoice.
    #[test]
    fn exactness_is_lost_and_never_regained() {
        let mut rec = StageRecord::new("gather".to_string(), 0);
        let reported = StageCall {
            cost_usd: Some(0.5),
            cost_reported: true,
            ..Default::default()
        };
        rec.record_call(&reported, 100);
        assert!(rec.cost_is_exact);
        rec.record_call(&computed(0.5), 110);
        assert!(!rec.cost_is_exact);
        rec.record_call(&reported, 120);
        assert!(!rec.cost_is_exact, "one reconstruction taints the total");
    }

    /// Each stay gets its own line. The accumulated record is the sum, which is
    /// the figure a graph of the run's path cannot use: a stage entered twice is
    /// two nodes, and attributing the sum to either of them is wrong.
    #[test]
    fn a_revisited_stage_splits_its_cost_by_visit() {
        let mut rec = StageRecord::new("gather".to_string(), 0);
        rec.begin_visit(100);
        rec.record_call(&computed(0.25), 101);
        rec.close_visit(110);

        rec.begin_visit(200);
        rec.record_call(&computed(0.75), 201);

        assert_eq!(rec.cost_usd, Some(1.0), "the stage is still the sum");
        assert_eq!(rec.visit_count, 2);
        assert_eq!(rec.visits.len(), 2);
        assert_eq!(rec.visits[0].cost_usd, Some(0.25));
        assert_eq!(rec.visits[0].left_at, Some(110));
        assert_eq!(rec.visits[1].cost_usd, Some(0.75));
        assert_eq!(rec.visits[1].left_at, None, "still in it");
        assert_eq!(rec.visits[1].prompt_tokens, 10, "not the stage's 20");
    }

    /// Closing twice is what a persist tick does to every stage the run is not
    /// in, so the second call must not move the boundary or reopen anything.
    #[test]
    fn closing_a_visit_is_idempotent_and_a_call_reopens_nothing() {
        let mut rec = StageRecord::new("gather".to_string(), 0);
        rec.begin_visit(100);
        rec.close_visit(110);
        rec.close_visit(400);
        assert_eq!(rec.visits.len(), 1);
        assert_eq!(rec.visits[0].left_at, Some(110), "the first close stands");

        // A call landing after the close belongs to a stay the ledger did not
        // see opened. It opens one rather than being folded into the visit that
        // has already ended, which would backdate money into a closed stay.
        rec.record_call(&computed(0.5), 420);
        assert_eq!(rec.visits.len(), 2);
        assert_eq!(rec.visits[1].entered_at, 420);
        assert_eq!(rec.visits[0].cost_usd, Some(0.0));
    }

    /// Past the cap the stage's own figures stay exact and the list stops
    /// growing, because `stages.json` is rewritten whole on every persist tick.
    /// `visit_count` keeps counting, which is the only signal that the split a
    /// reader is looking at is partial.
    #[test]
    fn the_visit_list_is_capped_while_the_count_is_not() {
        let mut rec = StageRecord::new("loop".to_string(), 0);
        for i in 0..(MAX_STAGE_VISITS + 20) {
            let at = 100 + i as i64;
            rec.begin_visit(at);
            rec.record_call(&computed(0.01), at);
            rec.close_visit(at + 1);
        }
        assert_eq!(rec.visits.len(), MAX_STAGE_VISITS);
        assert_eq!(rec.visit_count, MAX_STAGE_VISITS + 20);
        // Every call is still in the stage's own total, cap or no cap.
        let expected = (MAX_STAGE_VISITS + 20) as f64 * 0.01;
        assert!((rec.cost_usd.expect("priced") - expected).abs() < 1e-9);
        // And a call arriving past the cap does not restart the counter by
        // lazily opening a visit per call.
        let before = rec.visit_count;
        rec.record_call(&computed(0.01), 9_000);
        rec.record_call(&computed(0.01), 9_001);
        assert_eq!(rec.visit_count, before, "calls are not visits");
        assert_eq!(rec.visits.len(), MAX_STAGE_VISITS);
    }

    /// A visit keeps a working clock on the same rule the stage does: a run
    /// parked on a person is in the stage and not working.
    #[test]
    fn a_visits_clock_measures_work_rather_than_the_stay() {
        let mut rec = StageRecord::new("gather".to_string(), 0);
        rec.begin_visit(100);
        rec.observe_visit(100, true);
        rec.observe_visit(130, false); // parked
        rec.observe_visit(500, true); // back to work
        rec.close_visit(520);
        assert_eq!(rec.visits[0].active_runtime_secs(9_999), 50);

        // A record written before the clock existed falls back to the stay,
        // which is the only thing it recorded.
        let mut old = StageVisitRecord::opened_at(100);
        old.left_at = Some(160);
        old.active = None;
        assert_eq!(old.active_runtime_secs(9_999), 60);
    }

    /// The persist tick runs the clock forward on every stage, every tick,
    /// including the ones with nothing open. Neither case may invent a visit or
    /// restart a finished one's clock: a stage the run has not reached would
    /// acquire a stay it never had, and a stage it has left would go on billing
    /// working time for the rest of the run.
    #[test]
    fn running_the_clock_on_a_stage_with_nothing_open_changes_nothing() {
        let mut never_entered = StageRecord::new("review".to_string(), 2);
        never_entered.observe_visit(500, true);
        assert!(never_entered.visits.is_empty());

        let mut left = StageRecord::new("gather".to_string(), 0);
        left.begin_visit(100);
        left.observe_visit(100, true);
        left.close_visit(140);
        left.observe_visit(9_000, true);
        assert_eq!(left.visits[0].active_runtime_secs(99_999), 40);
        assert_eq!(left.visits[0].left_at, Some(140));
    }

    /// The whole record survives `stages.json`, visits included, and a file
    /// written before the cost fields existed still parses - as unknown-shaped
    /// defaults, not as an error.
    #[test]
    fn a_stage_record_with_visits_survives_the_file_it_lives_in() {
        let mut rec = StageRecord::new("gather".to_string(), 1);
        rec.begin_visit(100);
        rec.record_call(&computed(0.25), 101);
        let json = serde_json::to_string(&rec).unwrap();
        let back: StageRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cost_usd, Some(0.25));
        assert_eq!(back.visits.len(), 1);
        assert_eq!(back.visits[0].cost_usd, Some(0.25));
        assert_eq!(back.visit_count, 1);

        let old = r#"{"name":"gather","index":1,"status":"complete",
            "prompt_tokens":10,"completion_tokens":2,
            "started_at":null,"ended_at":null}"#;
        let back: StageRecord = serde_json::from_str(old).unwrap();
        assert_eq!(back.cost_usd, None, "no field is not a zero");
        assert!(back.visits.is_empty());
        assert_eq!(back.visit_count, 0);
    }
}
