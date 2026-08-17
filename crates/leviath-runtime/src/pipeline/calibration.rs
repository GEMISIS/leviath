//! Correcting the window's estimate against what the provider actually charged.
//!
//! The context window sizes everything it holds with
//! [`leviath_core::estimate_tokens`], which is bytes divided by four. That is a
//! guess, and it is a guess in a place where being wrong is expensive: region
//! budgets, the eviction trigger and the output cap are all decided against it,
//! and on a provider whose window is a hard ceiling the arithmetic being a few
//! percent light is the difference between a run that finishes and a run that
//! dies.
//!
//! The guess is also correctable, because every response says what the request
//! really cost. `prompt_tokens` comes back from the server's own tokenizer on
//! every call and is already journaled. Comparing it against what the window
//! believed the same request would cost gives the drift directly, so the
//! runtime does not have to guess better - it only has to listen.
//!
//! What is measured is end-to-end rather than a pure tokenizer ratio: the
//! reported figure covers the tool schemas and the hint blocks as well as the
//! regions, and those cost just as much. What the eviction trigger needs is one
//! number carrying "what the window thinks it holds" over to "what the provider
//! will charge", and that is what this keeps.
//!
//! Two properties keep this from being a trade:
//!
//! * It only ever tightens. A provider that charges less than the estimate -
//!   which is the common case, since `len()` counts bytes and any non-ASCII
//!   text inflates the estimate - leaves the correction at zero and changes
//!   nothing at all. Nobody's usable context shrinks unless their provider was
//!   measured charging more than the runtime believed.
//! * It only engages on measured evidence. There is no margin here, no
//!   guessed percentage held back from every workload on the chance that some
//!   of them drift. Until a call is observed costing more, the arithmetic is
//!   exactly what it was.
//!
//! Issue #485: a 27B model pinned to `num_ctx 32768` assembled 32,497 real
//! tokens on a Python-heavy corpus while the estimator believed it was inside
//! the region budgets. The next call appended three more read results, crossed
//! the window, and Ollama front-truncated from the start - taking the last user
//! turn with it and answering `no user query found in messages`, which names
//! neither the size nor the truncation.

use bevy_ecs::prelude::Component;

/// What the window believed the request just dispatched would cost.
///
/// Written at dispatch and read when the response lands, which is the only
/// place both halves of the comparison exist at once. Kept as its own component
/// rather than folded into [`PromptCalibration`] because dispatch overwrites it
/// wholesale on every call while the calibration accumulates across them.
#[derive(Component, Debug, Clone, Copy)]
pub struct PromptEstimate(pub usize);

/// How many tokens a request costs beyond what the window accounted for.
///
/// Additive rather than a ratio, and that distinction is the whole design.
/// What separates the reported figure from the window's estimate is two
/// different things added together:
///
/// * **Overhead the window never sees** - the tool schemas, the framework hint
///   blocks, the provider's own message framing. It is roughly constant for a
///   stage and does not grow when a region does.
/// * **Tokenizer drift** - the real error in bytes-over-four, which *is*
///   proportional to content.
///
/// A ratio models the second and mangles the first. Measured live on a small
/// window: a 26-token context against a 466-token request is a ratio of 18,
/// which as a multiplier would pull the eviction trigger down to a fifth of the
/// window and evict continuously - when the honest reading is "this stage
/// carries 440 tokens of schema". Because the correction is a high-water mark,
/// an agent that measured that while nearly empty would never recover from it.
///
/// Adding instead gets the overhead exactly right and tracks drift one call
/// behind, which the eviction threshold's own margin absorbs.
///
/// One figure per agent rather than per model: an agent's stages can name
/// different models, but they share a window, and it is the window's arithmetic
/// being corrected.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct PromptCalibration {
    /// The largest gap yet seen between what a request was charged and what the
    /// window believed it held.
    shortfall: usize,
}

impl PromptCalibration {
    /// Tokens to add to the window's estimate to get what will really be
    /// charged.
    pub fn shortfall(&self) -> usize {
        self.shortfall
    }

    /// Fold in one call's reported cost against what was estimated for it.
    ///
    /// The high-water mark rather than an average or a decay. The failure being
    /// prevented is absorbing - a run that overflows a hard window dies, and
    /// dies again on every retry, because the content that overflowed is still
    /// in the window. The failure being risked by holding the maximum is that
    /// eviction fires earlier than it strictly had to, which is a mechanism
    /// that already runs on every long run and drops the regions marked
    /// droppable first. Those are not the same size of mistake.
    ///
    /// Returns whether the correction moved, so the caller can say so once
    /// rather than on every call.
    pub fn observe(&mut self, estimated: usize, reported: usize) -> bool {
        // A provider that reported no usage at all is not evidence that the
        // request was free.
        if reported == 0 {
            return false;
        }
        // Charged less than estimated is the ordinary case on any text with
        // non-ASCII in it, since the estimator counts bytes. Nothing to correct.
        let Some(gap) = reported.checked_sub(estimated) else {
            return false;
        };
        if gap <= self.shortfall {
            return false;
        }
        self.shortfall = gap;
        true
    }
}

/// The correction for an agent that may not have been calibrated yet.
///
/// Absent means an agent spawned before this existed, or one in a test that
/// builds its components by hand. Both should behave exactly as they did.
pub(crate) fn shortfall_of(calibration: Option<&PromptCalibration>) -> usize {
    calibration.map_or(0, PromptCalibration::shortfall)
}

/// What `estimated` tokens are really expected to cost.
pub(crate) fn calibrated_tokens(
    estimated: usize,
    calibration: Option<&PromptCalibration>,
) -> usize {
    estimated.saturating_add(shortfall_of(calibration))
}

/// Whether the window has reached `threshold` of its budget, measured in what
/// the provider will charge rather than in what the estimator believed.
///
/// Correcting the reading rather than the window's `max_tokens`: the region
/// budgets were resolved against `max_tokens` at spawn, and moving it under a
/// live window would leave regions retroactively over budget.
pub(crate) fn needs_eviction_calibrated(
    current_tokens: usize,
    max_tokens: usize,
    threshold: f32,
    calibration: Option<&PromptCalibration>,
) -> bool {
    if max_tokens == 0 {
        return false;
    }
    let corrected = calibrated_tokens(current_tokens, calibration);
    corrected as f32 / max_tokens as f32 >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_uncalibrated_agent_changes_nothing() {
        assert_eq!(shortfall_of(None), 0);
        assert_eq!(calibrated_tokens(1000, None), 1000);
        assert!(!needs_eviction_calibrated(8_000, 10_000, 0.9, None));
        assert!(needs_eviction_calibrated(9_000, 10_000, 0.9, None));
    }

    #[test]
    fn a_fresh_calibration_changes_nothing_either() {
        let cal = PromptCalibration::default();
        assert_eq!(cal.shortfall(), 0);
        assert_eq!(calibrated_tokens(1000, Some(&cal)), 1000);
        assert!(!needs_eviction_calibrated(8_000, 10_000, 0.9, Some(&cal)));
    }

    /// The reporter's numbers: a window that believed it held 29,491 and was
    /// charged 32,497 for it.
    #[test]
    fn a_provider_that_charges_more_than_estimated_tightens_the_window() {
        let mut cal = PromptCalibration::default();
        cal.observe(29_491, 32_497);

        assert_eq!(cal.shortfall(), 3_006);
        assert_eq!(calibrated_tokens(10_000, Some(&cal)), 13_006);
    }

    /// The common case. `estimate_tokens` counts bytes, so any non-ASCII text
    /// reads as larger than it is charged - and nothing about that needs fixing.
    #[test]
    fn a_provider_that_charges_less_than_estimated_is_left_alone() {
        let mut cal = PromptCalibration::default();
        assert!(!cal.observe(10_000, 6_000));

        assert_eq!(cal.shortfall(), 0);
        assert_eq!(calibrated_tokens(10_000, Some(&cal)), 10_000);
    }

    /// The bug live testing found. A stage carrying big tool schemas against a
    /// nearly empty window reports a ratio of eighteen; as a multiplier that
    /// pulled the eviction trigger down to a fifth of the window and, being a
    /// high-water mark, never let go of it. The honest reading is 440 tokens of
    /// schema, which costs 440 tokens at every size.
    #[test]
    fn fixed_overhead_does_not_scale_with_the_window() {
        let mut cal = PromptCalibration::default();
        cal.observe(26, 466);

        assert_eq!(cal.shortfall(), 440);
        // The correction it implies once the window is actually holding
        // something: still 440, not eighteen times everything.
        assert_eq!(calibrated_tokens(20_000, Some(&cal)), 20_440);
        assert!(
            !needs_eviction_calibrated(20_000, 100_000, 0.9, Some(&cal)),
            "a fifth-full window is not evicting"
        );
    }

    #[test]
    fn the_correction_holds_its_high_water_mark() {
        let mut cal = PromptCalibration::default();
        cal.observe(1_000, 1_300);
        cal.observe(1_000, 1_050);

        assert_eq!(
            cal.shortfall(),
            300,
            "a friendlier call does not forget the drift"
        );
    }

    /// The crossing is reported so the runtime can say so once. A steady run
    /// that never drifts further must stay quiet.
    #[test]
    fn only_a_correction_that_moved_reports_itself() {
        let mut cal = PromptCalibration::default();

        assert!(cal.observe(1_000, 1_200), "the first shortfall is news");
        assert!(!cal.observe(1_000, 1_100), "a friendlier call is not");
        assert!(cal.observe(1_000, 1_500), "a worse one is news again");
        assert!(!cal.observe(1_000, 0), "no reported usage is not evidence");
    }

    /// The end the whole thing exists for, on the reporter's own numbers: a
    /// 32,768 window and an estimator running about 3,000 tokens light.
    ///
    /// The eviction threshold exists to leave a margin between "nearly full"
    /// and "over the window", and 0.9 of 32,768 is meant to be 3,277 tokens of
    /// it. Measured against the real tokenizer that margin was 269 - less than
    /// a single tool result, so the very next append crossed the window
    /// whatever eviction did. The threshold had not stopped working; it was
    /// being applied to a number that was not the one that mattered.
    #[test]
    fn eviction_leaves_room_to_evict_into_rather_than_a_sliver() {
        const WINDOW: usize = 32_768;
        const SHORTFALL: usize = 3_006;

        let mut cal = PromptCalibration::default();
        cal.observe(29_491, 29_491 + SHORTFALL);

        // Uncalibrated, the trigger fires with almost nothing left in front of
        // it: 269 real tokens, less than a single tool result, so the next
        // append crosses the window whatever eviction does.
        let raw_trip = (WINDOW as f32 * 0.9) as usize;
        let raw_margin = WINDOW - (raw_trip + SHORTFALL);
        assert!(
            raw_margin < 1_000,
            "the bug: only {raw_margin} real tokens of margin at the trigger"
        );

        // Calibrated, the same threshold fires while there is still room.
        assert!(
            needs_eviction_calibrated(raw_trip, WINDOW, 0.9, Some(&cal)),
            "the fix: the trigger reads the corrected figure"
        );
        let margin = WINDOW - calibrated_tokens(WINDOW * 8 / 10, Some(&cal));
        assert!(
            margin > 3_000,
            "an eight-tenths window still has {margin} real tokens in front of it"
        );
    }

    #[test]
    fn a_window_with_no_budget_never_evicts() {
        assert!(!needs_eviction_calibrated(100, 0, 0.9, None));
    }

    #[test]
    fn the_estimate_component_carries_what_was_believed() {
        let estimate = PromptEstimate(4_096);
        assert_eq!(estimate.0, 4_096);
        assert_eq!(format!("{estimate:?}"), "PromptEstimate(4096)");
    }

    #[test]
    fn a_calibration_is_inspectable() {
        let mut cal = PromptCalibration::default();
        cal.observe(1_000, 1_500);
        let shown = format!("{cal:?}");
        assert!(shown.contains("500"), "the shortfall is legible: {shown}");
        assert_eq!(cal.shortfall(), PromptCalibration::clone(&cal).shortfall());
    }
}
