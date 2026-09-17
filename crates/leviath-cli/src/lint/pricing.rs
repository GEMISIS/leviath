//! A note for a stage whose context can grow past a model's long-context
//! price threshold.
//!
//! Several vendors bill a whole request at a higher rate once its prompt
//! reaches a size (200 000 tokens on Gemini 2.5 Pro and Claude Sonnet 4, for
//! example). Nothing is wrong with a stage that gets there, and model
//! selection never looks at it, but a run that crosses it costs noticeably
//! more per turn, so the author hears about it at the one moment they are
//! reading the blueprint.

use leviath_core::blueprint::Blueprint;

use super::{LintEnv, LintFinding, LintSeverity};

/// The note for each model a stage names whose long-context tier its context
/// budget can reach, once per stage and model.
pub(super) fn lint_long_context_price(blueprint: &Blueprint, env: &LintEnv) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    for stage in &blueprint.stages {
        let layout = stage
            .context_layout
            .as_ref()
            .unwrap_or(&blueprint.context_layout);
        for entry in &stage.model.models {
            let key = (entry.provider.clone(), entry.model.clone());
            let Some(window) = env.model_windows.get(&key) else {
                continue;
            };
            let Some((pricing, tier)) =
                leviath_providers::pricing::published_rates(&entry.provider, &entry.model)
                    .and_then(|p| p.long_context.map(|tier| (p, tier)))
            else {
                continue;
            };
            let budget: usize = layout
                .regions
                .iter()
                .map(|region| region.budget.resolve(*window))
                .sum::<usize>()
                .min(*window);
            if budget < tier.threshold_tokens {
                continue;
            }
            findings.push(
                LintFinding::new(
                    LintSeverity::Note,
                    "long-context-price",
                    format!(
                        "{}/{} bills a whole request at {:.2} in / {:.2} out per million \
                         tokens once its prompt reaches {} tokens (against {:.2} / {:.2} \
                         below), and this stage's context can grow to {budget}",
                        entry.provider,
                        entry.model,
                        tier.input_per_mtok,
                        tier.output_per_mtok,
                        tier.threshold_tokens,
                        pricing.input_per_mtok,
                        pricing.output_per_mtok,
                    ),
                )
                .in_stage(&stage.name)
                .with_fix(format!(
                    "nothing to fix; to stay under the higher rate, give the stage's \
                     regions a smaller total budget or a max_tokens that keeps it below \
                     {} tokens",
                    tier.threshold_tokens
                )),
            );
        }
    }
    findings
}
