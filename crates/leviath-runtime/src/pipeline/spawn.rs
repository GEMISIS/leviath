//! What the caller resolves for each stage of a blueprint, and the window a
//! stage is sized against when its provider is not registered.

use super::*;

/// A blueprint stage resolved to a concrete provider, model, and effective tool
/// set - the per-stage input to a spawn. The caller (CLI / daemon) owns
/// the model-selection policy (overrides, availability, user defaults) and tool
/// filtering; the runtime just turns the result into agent data.
#[derive(Debug)]
pub struct ResolvedStage {
    /// The provider to call for this stage.
    pub provider_name: String,
    /// The resolved model name.
    pub model: String,
    /// The effective tool set for this stage (already filtered).
    pub tools: Vec<Tool>,
    /// Where to go if `provider_name` turns out to be unusable, best first.
    /// See `crate::pipeline::resolve_stage_candidates`.
    pub fallbacks: Vec<crate::spec::names::ModelRef>,
    /// The output shape resolved for this stage: the blueprint's default, the
    /// stage's override, and the launching caller's request, combined. Resolved
    /// caller-side (like the model and tool choices beside it) because only the
    /// caller knows what was asked for at launch.
    pub output: Option<leviath_core::output::OutputSpec>,
    /// Operational lines to log for this stage at spawn: today, one per stage
    /// whose head the user's `override_model` or `fallback_model` moved off
    /// the blueprint's own choice. Empty when the blueprint's choice stands.
    pub notes: Vec<String>,
}

/// Fallback context window used when a stage's provider isn't registered (so
/// percentage budgets can't be resolved against a real model). Matches
/// [`leviath_providers::ModelCapabilities`]'s default `max_context_tokens`.
pub(crate) const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 8192;

#[cfg(test)]
mod stage_instructions_fit_tests {
    //! A stage prompt bigger than the first pinned region, spawned end to end.

    use crate::spec::graph::RegionDef;
    use crate::test_graph::pct;

    /// `regions` laid out for a window of `window` tokens: each one's
    /// percentage budget sized against it.
    fn laid_out(regions: &[RegionDef], window: usize) -> Vec<leviath_core::Region> {
        regions
            .iter()
            .map(|r| crate::context_setup::region_from_def(r, r.budget.resolve(window)))
            .collect()
    }

    /// A small `task` region beside a dedicated `stage_instructions` region
    /// with room for a stage prompt, laid out for a window of `window` tokens.
    fn layout(window: usize) -> Vec<leviath_core::Region> {
        laid_out(
            &[
                pct("task", 0.02),
                pct(crate::spec::graph::STAGE_INSTRUCTIONS_REGION, 0.03),
            ],
            window,
        )
    }

    /// A ~2.9k-token stage prompt: too big for 2% of a 128k window, comfortable
    /// in 3%.
    fn big_prompt() -> String {
        "word ".repeat(2_600)
    }

    #[test]
    fn a_stage_prompt_measured_at_spawn_uses_the_declared_region() {
        let window_tokens = 128_000;
        let layout = layout(window_tokens);
        let task_max = layout
            .iter()
            .find(|r| r.name == "task")
            .expect("task")
            .max_tokens;
        let instr_max = layout
            .iter()
            .find(|r| r.name == crate::spec::graph::STAGE_INSTRUCTIONS_REGION)
            .expect("stage_instructions")
            .max_tokens;
        let prompt = big_prompt();
        let tokens = leviath_core::estimate_tokens(&format!("[Stage instructions: {prompt}]"));
        assert!(
            tokens > task_max && tokens < instr_max,
            "the fixture must reproduce the reported shape: {tokens} vs task {task_max} / \
             stage_instructions {instr_max}"
        );

        let mut window = crate::components::ContextWindow::new(window_tokens);
        crate::context_setup::lay_out(&mut window, layout);
        let setup = crate::pipeline::transition::StageSetup {
            inference_config: crate::components::InferenceConfig {
                temperature: None,
                max_output_tokens: None,
                extra_params: Default::default(),
                batch_tool_hint: false,
                shell_hint: false,
                request_timeout_secs: None,
                as_text: Vec::new(),
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            eviction_order: Vec::new(),
            context_hide: Vec::new(),
            context_reset: Vec::new(),
            system_prompt: Some(prompt),
        };
        crate::pipeline::transition::apply_stage_context(&setup, &mut window)
            .expect("the prompt fits the region declared for it");

        let instr = window
            .get_region(crate::spec::graph::STAGE_INSTRUCTIONS_REGION)
            .expect("region exists");
        assert!(
            instr.content.iter().any(|e| e.content.contains("word")),
            "the prompt landed in stage_instructions"
        );
    }

    /// A blueprint that declares no `stage_instructions` region at all.
    ///
    /// Without one the prompt goes to `task` - the first pinned region, sized
    /// for a sentence from the caller - and on a small window the spawn dies
    /// with `stage system prompt does not fit region 'task'`. The alternative
    /// is flooring every task region with a `min_tokens` sized for the largest
    /// stage prompt, coupling an unrelated region to prompt lengths.
    #[test]
    fn a_blueprint_that_declares_no_region_still_gets_one() {
        let window_tokens = 128_000;
        let prompt = big_prompt();

        // Only `task`, at 2% - a region sized for a sentence from the caller.
        let only_task = laid_out(&[pct("task", 0.02)], window_tokens);

        let mut window = crate::components::ContextWindow::new(window_tokens);
        crate::context_setup::lay_out(&mut window, only_task);
        let prompts = vec![Some(prompt.clone())];
        crate::context_setup::ensure_stage_instructions_region(&mut window, &prompts);

        let setup = crate::pipeline::transition::StageSetup {
            inference_config: crate::components::InferenceConfig {
                temperature: None,
                max_output_tokens: None,
                extra_params: Default::default(),
                batch_tool_hint: false,
                shell_hint: false,
                request_timeout_secs: None,
                as_text: Vec::new(),
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            eviction_order: Vec::new(),
            context_hide: Vec::new(),
            context_reset: Vec::new(),
            system_prompt: Some(prompt),
        };
        crate::pipeline::transition::apply_stage_context(&setup, &mut window)
            .expect("the prompt no longer has to fit the caller's task region");

        let task_region = window.get_region("task").expect("task");
        assert!(
            task_region.content.is_empty(),
            "the task region is left for the caller's task"
        );
        let instr = window
            .get_region(crate::spec::graph::STAGE_INSTRUCTIONS_REGION)
            .expect("the runtime made one");
        assert!(instr.content.iter().any(|e| e.content.contains("word")));
    }

    /// Nothing to hold means no region: an empty pinned region is budget taken
    /// from the work for nothing.
    #[test]
    fn no_region_is_made_when_no_stage_has_a_prompt() {
        let mut window = crate::components::ContextWindow::new(1_000);
        crate::context_setup::ensure_stage_instructions_region(&mut window, &[None, None]);
        assert!(
            window
                .get_region(crate::spec::graph::STAGE_INSTRUCTIONS_REGION)
                .is_none()
        );
    }

    /// A declared region is left exactly as the author sized it.
    #[test]
    fn a_declared_region_is_not_resized() {
        let mut window = crate::components::ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            crate::spec::graph::STAGE_INSTRUCTIONS_REGION.to_string(),
            leviath_core::RegionKind::Pinned,
            4_242,
        ));
        crate::context_setup::ensure_stage_instructions_region(&mut window, &[Some(big_prompt())]);
        assert_eq!(
            window
                .get_region(crate::spec::graph::STAGE_INSTRUCTIONS_REGION)
                .expect("declared")
                .max_tokens,
            4_242
        );
    }

    /// A prompt bigger than the window is still a spawn failure - it was always
    /// going to be. What changes is that the message names the region the prompt
    /// was going to, rather than the caller's task region.
    #[test]
    fn an_impossible_prompt_is_still_refused_and_names_the_right_region() {
        let mut window = crate::components::ContextWindow::new(1_000);
        window.add_region(leviath_core::Region::new(
            "task".to_string(),
            leviath_core::RegionKind::Pinned,
            40,
        ));
        let prompt = "z".repeat(100_000);
        crate::context_setup::ensure_stage_instructions_region(
            &mut window,
            &[Some(prompt.clone())],
        );
        // Capped at a quarter of the window rather than sized to the prompt.
        assert_eq!(
            window
                .get_region(crate::spec::graph::STAGE_INSTRUCTIONS_REGION)
                .expect("made")
                .max_tokens,
            250
        );

        let setup = crate::pipeline::transition::StageSetup {
            inference_config: crate::components::InferenceConfig {
                temperature: None,
                max_output_tokens: None,
                extra_params: Default::default(),
                batch_tool_hint: false,
                shell_hint: false,
                request_timeout_secs: None,
                as_text: Vec::new(),
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            eviction_order: Vec::new(),
            context_hide: Vec::new(),
            context_reset: Vec::new(),
            system_prompt: Some(prompt),
        };
        let err = crate::pipeline::transition::apply_stage_context(&setup, &mut window)
            .expect_err("a prompt larger than the window cannot be housed");
        assert!(
            err.contains(crate::spec::graph::STAGE_INSTRUCTIONS_REGION),
            "{err}"
        );
    }

    /// Sized for the largest prompt in the blueprint, not the first stage's:
    /// every stage's instructions pass through the same region.
    #[test]
    fn the_region_is_sized_for_the_widest_prompt() {
        let mut window = crate::components::ContextWindow::new(100_000);
        let small = "word ".repeat(10);
        let large = big_prompt();
        let expected = leviath_core::estimate_tokens(&format!("[Stage instructions: {large}]"));
        crate::context_setup::ensure_stage_instructions_region(
            &mut window,
            &[Some(small), Some(large)],
        );
        assert_eq!(
            window
                .get_region(crate::spec::graph::STAGE_INSTRUCTIONS_REGION)
                .expect("made")
                .max_tokens,
            expected
        );
    }

    /// The reported shape: the stage carries its own `[context.regions]`, which
    /// does not re-declare `stage_instructions`.
    #[test]
    fn a_scoped_stage_layout_still_routes_to_the_declared_region() {
        let window_tokens = 128_000;
        let prompt = big_prompt();

        // The stage narrows what it attends to and says nothing about
        // stage_instructions - the region is the runtime's to fill.
        let scoped = laid_out(&[pct("task", 0.02)], window_tokens);

        let mut window = crate::components::ContextWindow::new(window_tokens);
        crate::context_setup::lay_out(&mut window, layout(window_tokens));
        let setup = crate::pipeline::transition::StageSetup {
            inference_config: crate::components::InferenceConfig {
                temperature: None,
                max_output_tokens: None,
                extra_params: Default::default(),
                batch_tool_hint: false,
                shell_hint: false,
                request_timeout_secs: None,
                as_text: Vec::new(),
            },
            routing: None,
            accepts_messages: true,
            context_layout: Some(scoped),
            eviction_order: Vec::new(),
            context_hide: Vec::new(),
            context_reset: Vec::new(),
            system_prompt: Some(prompt),
        };
        crate::pipeline::transition::apply_stage_context(&setup, &mut window)
            .expect("the prompt fits the region declared for it");

        let instr = window
            .get_region(crate::spec::graph::STAGE_INSTRUCTIONS_REGION)
            .expect("carried through the scoped layout");
        assert!(
            instr.content.iter().any(|e| e.content.contains("word")),
            "the prompt landed in stage_instructions, not in the scoped task region"
        );
    }
}
