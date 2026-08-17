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
//! What is measured here is deliberately end-to-end rather than a pure
//! tokenizer ratio: the reported figure covers the tool schemas and the hint
//! blocks as well as the regions, and those are just as real. One number that
//! maps "what the window thinks it holds" onto "what the provider will charge"
//! is what the eviction trigger needs, and it is what this produces.
//!
//! Two properties keep this from being a trade:
//!
//! * It only ever tightens. A provider that charges less than the estimate -
//!   which is the common case, since `len()` counts bytes and any non-ASCII
//!   text inflates the estimate - leaves the factor at one and changes nothing
//!   at all. Nobody's usable context shrinks unless their provider was measured
//!   charging more than the runtime believed.
//! * It only engages on measured evidence. There is no margin here, no
//!   guessed percentage held back from every workload on the chance that some
//!   of them drift. Until a call is observed drifting, the arithmetic is exactly
//!   what it was.
//!
//! Issue #485: a 27B model pinned to `num_ctx 32768` assembled 32,497 real
//! tokens on a Python-heavy corpus while the estimator believed it was inside
//! the region budgets. The next call appended three more read results, crossed
//! the window, and Ollama front-truncated from the start - taking the last user
//! turn with it and answering `no user query found in messages`, which names
//! neither the size nor the truncation.

use bevy_ecs::prelude::Component;

/// The largest correction that will be applied, as a multiple of the estimate.
///
/// A tokenizer that disagrees with bytes-over-four by more than this is not
/// drifting, it is measuring something else - an image part, or a schema the
/// request carries but the window never saw - and shrinking the usable window
/// to a quarter on the strength of one such call would be its own outage. The
/// cap keeps a single anomalous response from collapsing a run that is
/// otherwise fine.
const MAX_CALIBRATION: f64 = 4.0;

/// What the window believed the request just dispatched would cost.
///
/// Written at dispatch and read when the response lands, which is the only
/// place both halves of the comparison exist at once. Kept as its own component
/// rather than folded into [`PromptCalibration`] because dispatch overwrites it
/// wholesale on every call while the calibration accumulates across them.
#[derive(Component, Debug, Clone, Copy)]
pub struct PromptEstimate(pub usize);

/// How far this agent's estimate falls short of what its provider charges.
///
/// One factor per agent rather than per model: an agent's stages can name
/// different models, but they share a window, and it is the window's arithmetic
/// being corrected. A stage that moves to a model with a friendlier tokenizer
/// keeps the tighter factor, which costs it some headroom and cannot cost it a
/// run.
#[derive(Component, Debug, Clone, Copy)]
pub struct PromptCalibration {
    /// Reported over estimated, at the highest yet observed, never below one.
    factor: f64,
}

impl Default for PromptCalibration {
    fn default() -> Self {
        Self { factor: 1.0 }
    }
}

impl PromptCalibration {
    /// The correction to apply to an estimate, as a multiplier of at least one.
    pub fn factor(&self) -> f64 {
        self.factor
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
    pub fn observe(&mut self, estimated: usize, reported: usize) {
        // A call with nothing to compare says nothing. Zero estimated would
        // divide by zero; zero reported is a provider that did not report usage
        // at all, and reading that as "this request was free" would drive the
        // factor down - except that it cannot, because the factor only rises.
        if estimated == 0 || reported == 0 {
            return;
        }
        let observed = reported as f64 / estimated as f64;
        if observed > self.factor {
            self.factor = observed.min(MAX_CALIBRATION);
        }
    }
}

/// The factor to apply, for an agent that may not have been calibrated yet.
///
/// Absent means an agent spawned before this existed, or one in a test that
/// builds its components by hand. Both should behave exactly as they did.
pub(crate) fn factor_of(calibration: Option<&PromptCalibration>) -> f64 {
    calibration.map_or(1.0, PromptCalibration::factor)
}

/// What `estimated` tokens are really expected to cost.
///
/// Rounded up, so the correction is never rounded away on a small region.
pub(crate) fn calibrated_tokens(
    estimated: usize,
    calibration: Option<&PromptCalibration>,
) -> usize {
    let factor = factor_of(calibration);
    if factor <= 1.0 {
        return estimated;
    }
    // f64 carries every usize a context window can hold without loss, and the
    // factor is bounded by MAX_CALIBRATION, so the product cannot leave range.
    (estimated as f64 * factor).ceil() as usize
}

/// The fill threshold that means the same thing in real tokens as `base` did in
/// estimated ones.
///
/// Scaling the trigger rather than the window's `max_tokens`: the region
/// budgets were resolved against `max_tokens` at spawn, and moving it under a
/// live window would leave regions retroactively over budget. The threshold is
/// read fresh on every tick and belongs to nothing else.
pub(crate) fn calibrated_threshold(base: f32, calibration: Option<&PromptCalibration>) -> f32 {
    let factor = factor_of(calibration);
    if factor <= 1.0 {
        return base;
    }
    base / factor as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_uncalibrated_agent_changes_nothing() {
        assert_eq!(factor_of(None), 1.0);
        assert_eq!(calibrated_tokens(1000, None), 1000);
        assert_eq!(calibrated_threshold(0.9, None), 0.9);
    }

    #[test]
    fn a_fresh_calibration_changes_nothing_either() {
        let cal = PromptCalibration::default();
        assert_eq!(cal.factor(), 1.0);
        assert_eq!(calibrated_tokens(1000, Some(&cal)), 1000);
        assert_eq!(calibrated_threshold(0.9, Some(&cal)), 0.9);
    }

    /// The reporter's numbers: a window that believed it held 29,491 and was
    /// charged 32,497 for it.
    #[test]
    fn a_provider_that_charges_more_than_estimated_tightens_the_window() {
        let mut cal = PromptCalibration::default();
        cal.observe(29_491, 32_497);

        assert!(cal.factor() > 1.10, "measured drift is about 10%");
        assert!(cal.factor() < 1.11);
        // Eviction has to start early enough that the freed space arrives
        // before the real window does.
        let threshold = calibrated_threshold(0.9, Some(&cal));
        assert!(threshold < 0.82, "0.9 of a window 10% larger than believed");
        assert_eq!(calibrated_tokens(10_000, Some(&cal)), 11_020);
    }

    /// The common case. `estimate_tokens` counts bytes, so any non-ASCII text
    /// reads as larger than it is charged - and nothing about that needs fixing.
    #[test]
    fn a_provider_that_charges_less_than_estimated_is_left_alone() {
        let mut cal = PromptCalibration::default();
        cal.observe(10_000, 6_000);

        assert_eq!(cal.factor(), 1.0);
        assert_eq!(calibrated_tokens(10_000, Some(&cal)), 10_000);
        assert_eq!(calibrated_threshold(0.9, Some(&cal)), 0.9);
    }

    #[test]
    fn the_correction_holds_its_high_water_mark() {
        let mut cal = PromptCalibration::default();
        cal.observe(1_000, 1_300);
        cal.observe(1_000, 1_050);

        let factor = cal.factor();
        assert!(
            (factor - 1.3).abs() < 1e-9,
            "a friendlier call does not forget the drift: {factor}"
        );
    }

    #[test]
    fn one_anomalous_call_cannot_collapse_the_window() {
        let mut cal = PromptCalibration::default();
        cal.observe(1_000, 100_000);

        assert_eq!(cal.factor(), MAX_CALIBRATION);
    }

    #[test]
    fn a_call_with_nothing_to_compare_is_ignored() {
        let mut cal = PromptCalibration::default();
        cal.observe(0, 5_000);
        cal.observe(5_000, 0);

        assert_eq!(cal.factor(), 1.0);
    }

    #[test]
    fn the_estimate_component_carries_what_was_believed() {
        let estimate = PromptEstimate(4_096);
        assert_eq!(estimate.0, 4_096);
        assert_eq!(format!("{estimate:?}"), "PromptEstimate(4096)");
    }

    /// The end the whole thing exists for, on the reporter's own numbers: a
    /// 32,768 window and an estimator running about 10% light.
    ///
    /// The eviction threshold exists to leave a margin between "nearly full"
    /// and "over the window", and 0.9 of 32,768 is meant to be 3,277 tokens of
    /// it. Measured against the real tokenizer that margin is 269 - less than a
    /// single tool result, so the very next append crosses the window whatever
    /// eviction does. The threshold had not stopped working; it was being
    /// applied to a number that was not the one that mattered.
    ///
    /// The 32,499 this computes is the same figure the reporter's journal
    /// recorded for the last call that succeeded, which is the corroboration
    /// that the drift is what it looks like.
    #[test]
    fn eviction_leaves_room_to_evict_into_rather_than_a_sliver() {
        const WINDOW: usize = 32_768;
        const DRIFT: f64 = 1.102;
        /// A read result landing in the window, which is what the run appended
        /// next.
        const ONE_TOOL_RESULT: usize = 1_000;

        let real = |estimated: usize| (estimated as f64 * DRIFT) as usize;
        let headroom_at = |threshold: f32| {
            let trip = (WINDOW as f32 * threshold) as usize;
            WINDOW.saturating_sub(real(trip))
        };

        let uncalibrated = headroom_at(0.9);
        assert!(
            uncalibrated < ONE_TOOL_RESULT,
            "the bug: {uncalibrated} real tokens of margin, and the next append is larger"
        );

        let mut cal = PromptCalibration::default();
        let believed = WINDOW / 2;
        cal.observe(believed, real(believed));
        let calibrated = headroom_at(calibrated_threshold(0.9, Some(&cal)));
        assert!(
            calibrated > ONE_TOOL_RESULT * 3,
            "the fix: {calibrated} real tokens of margin, which eviction can work with"
        );
    }

    #[test]
    fn a_calibration_is_inspectable() {
        let mut cal = PromptCalibration::default();
        cal.observe(1_000, 1_500);
        let shown = format!("{cal:?}");
        assert!(shown.contains("1.5"), "the factor is legible: {shown}");
        assert_eq!(cal.factor(), PromptCalibration::clone(&cal).factor());
    }
}
