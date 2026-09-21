//! `Stage`: one step of a blueprint, as its author declared it.
//!
//! Every field is what the manifest says, not what a run resolved. A setting the
//! stage leaves out is null here even where the daemon has a default for it, and
//! `effective` is where the resolved answer lives.

use std::sync::Arc;

use async_graphql::{Enum, Object, SimpleObject};

use leviath_core::Blueprint as CoreBlueprint;

use super::super::blueprint::ToolUseGuidance;
use super::count;
use super::interaction::InteractionPoint;
use super::model::StageModelConfig;
use super::output::{OutputSpec, StageInput};
use super::runtime::{
    BlueprintSecurity, NudgeConfig, SandboxConfig, StageHooks, WorkerFailurePolicy,
};
use super::tools::{OutputRoute, ToolAcceptRule, ToolPermissionRule, ToolRouting};
use super::transition::TransitionEdge;

/// How a stage runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum StageMode {
    /// The tight loop: infer, act on tool calls, repeat until a transition
    /// fires.
    Autonomous,
    /// Pauses for a person at every step.
    Interactive,
    /// Holds at each declared interaction point instead of at every step.
    InteractivePoints,
    /// Splits work across worker runs, then continues.
    FanOut,
    /// Produces the run's final answer and nothing else.
    Output,
}

impl From<&leviath_core::blueprint::StageMode> for StageMode {
    fn from(mode: &leviath_core::blueprint::StageMode) -> Self {
        use leviath_core::blueprint::StageMode as Core;
        match mode {
            Core::Autonomous => Self::Autonomous,
            Core::Interactive => Self::Interactive,
            Core::InteractivePoints { .. } => Self::InteractivePoints,
            Core::FanOut { .. } => Self::FanOut,
            Core::Output => Self::Output,
        }
    }
}

/// What a stage must produce before it may transition.
#[derive(Debug, SimpleObject)]
pub(crate) struct OutputRequirement {
    /// How many times the stage is asked again when it tries to leave without
    /// having submitted. After that the transition proceeds anyway and the
    /// run's `outputForced` counter records it: a missing answer never strands
    /// a run.
    pub(crate) reasks: i32,
}

/// What a stage's fan-out splits into, and how.
///
/// Only a `FAN_OUT` stage has one. Every other mode answers null, rather than a
/// block of defaults nobody wrote.
#[derive(Debug, SimpleObject)]
pub(crate) struct FanOut {
    /// A separate installed blueprint run as the worker, by name. It has to be
    /// installed when the fan-out runs, and it may not be installed now, so this
    /// is a name rather than a blueprint.
    pub(crate) worker_agent: Option<String>,
    /// A stage of this same blueprint run as the worker, by name. That stage has
    /// to allow it.
    pub(crate) worker_stage: Option<String>,
    /// A description matched against the installed blueprints, when the manifest
    /// would rather describe the worker than name it.
    pub(crate) worker_query: Option<String>,
    /// The stage that reconciles what the workers sent back, by name.
    pub(crate) merge_stage: Option<String>,
    /// The prompt that produces the work items.
    pub(crate) split_prompt: String,
    /// How many workers run at once. Zero means as many as the daemon's
    /// inference pool will carry.
    pub(crate) max_workers: i32,
    /// The most work items the split may produce. Null means however many it
    /// produces. This bounds what there is at all, where `maxWorkers` bounds how
    /// many run together, and it is also what decides each worker's share of the
    /// results region.
    pub(crate) max_items: Option<i32>,
    /// How many times the stage is asked again when it ends without having
    /// fanned out, before it is let through with no workers.
    pub(crate) max_attempts: Option<i32>,
    /// What happens when one worker fails.
    pub(crate) on_worker_failure: WorkerFailurePolicy,
    /// The region the consolidated report is written to, by name. Null means the
    /// conversation, which is a sliding window, so a bulky report is worth its
    /// own region and its own budget.
    pub(crate) results_region: Option<String>,
}

impl From<&leviath_core::blueprint::FanOutConfig> for FanOut {
    fn from(config: &leviath_core::blueprint::FanOutConfig) -> Self {
        Self {
            worker_agent: config.worker_agent.clone(),
            worker_stage: config.worker_stage.clone(),
            worker_query: config.worker_query.clone(),
            merge_stage: config.merge_stage.clone(),
            split_prompt: config.split_prompt.clone(),
            max_workers: count(config.max_workers),
            max_items: config.max_items.map(count),
            max_attempts: config.max_attempts.map(count),
            on_worker_failure: WorkerFailurePolicy::from(&config.on_worker_failure),
            results_region: config.results_region.clone(),
        }
    }
}

/// Which regions a stage adds, hides or empties on its way in.
#[derive(Debug, SimpleObject)]
pub(crate) struct StageContext {
    /// Regions this stage declares of its own, beyond the blueprint's layout, by
    /// name. Look them up in the blueprint's `regions`.
    pub(crate) regions: Vec<String>,
    /// Regions kept out of this stage's prompt, by name. The contents survive;
    /// the stage simply does not see them.
    pub(crate) hide: Vec<String>,
    /// Regions emptied as this stage is entered, by name.
    pub(crate) reset: Vec<String>,
}

/// One stage of a blueprint.
pub(crate) struct Stage {
    /// The blueprint this stage belongs to, shared rather than copied.
    pub(crate) blueprint: Arc<CoreBlueprint>,
    /// Which stage, by declaration order.
    pub(crate) at: usize,
}

#[Object]
impl Stage {
    /// Stage name, unique within the blueprint.
    async fn name(&self) -> &str {
        &self.stage().name
    }

    /// How the stage runs.
    async fn mode(&self) -> StageMode {
        StageMode::from(&self.stage().mode)
    }

    /// One line on what the stage is for.
    async fn description(&self) -> Option<&str> {
        self.stage().description.as_deref()
    }

    /// Extra prompt text for when the stage is choosing where to go next.
    async fn transition_prompt(&self) -> Option<&str> {
        self.stage().transition_prompt.as_deref()
    }

    /// Which model this stage runs on, and what it asks of it.
    async fn model(&self) -> StageModelConfig {
        StageModelConfig::from(&self.stage().model)
    }

    /// The only tools the model is offered here. Group tokens (`@all`,
    /// `@builtin`, `@subagent`, `@scripts`, `@mcp`) stand for whole sources and
    /// resolve at spawn.
    ///
    /// Names rather than tools: a blueprint may name a tool this machine does
    /// not have, and resolving would drop it. Read `tools` for what is here.
    async fn available_tools(&self) -> &[String] {
        &self.stage().available_tools
    }

    /// Tools this stage cannot work without. These survive an unattended run,
    /// where the blocking interaction tools are otherwise withheld.
    async fn required_tools(&self) -> &[String] {
        &self.stage().required_tools
    }

    /// MCP servers whose whole tool set this stage may use.
    async fn available_connectors(&self) -> &[String] {
        &self.stage().available_connectors
    }

    /// Inference-turn bound for one visit to this stage.
    async fn max_iterations(&self) -> Option<i32> {
        self.stage().max_iterations.map(count)
    }

    /// How many times the run may re-enter this stage.
    async fn max_revisits(&self) -> Option<i32> {
        self.stage().max_revisits.map(count)
    }

    /// What this stage must submit before it transitions. Null when it may
    /// leave without producing anything.
    async fn output_requirement(&self) -> Option<OutputRequirement> {
        self.stage().require_output.then(|| OutputRequirement {
            reasks: count(leviath_core::blueprint::DEFAULT_OUTPUT_REENTRY_CAP),
        })
    }

    /// The shape this stage's answer must take, narrowing the blueprint's own.
    /// Declaring a shape is not demanding one: that is `outputRequirement`.
    async fn output(&self) -> Option<OutputSpec> {
        self.stage().output.as_ref().map(OutputSpec::from)
    }

    /// What this stage takes as typed parts, when its regions do not already
    /// say.
    async fn input(&self) -> StageInput {
        StageInput {
            accepts: self.stage().input_accepts.clone(),
            as_text: self.stage().input_as_text.clone(),
        }
    }

    /// Whether `sendMessage` reaches a run parked in this stage.
    async fn accepts_messages(&self) -> bool {
        self.stage().accepts_messages
    }

    /// Whether this stage may end the run outright.
    async fn allow_complete(&self) -> bool {
        self.stage().allow_complete
    }

    /// Whether this stage may run as a fan-out worker or sub-agent.
    async fn allow_as_worker(&self) -> bool {
        self.stage().allow_as_worker
    }

    /// Whether this stage may not finish until its child runs complete.
    async fn requires_children(&self) -> bool {
        self.stage().requires_children
    }

    /// Records that the author deliberately offers the human-in-the-loop tools
    /// while this stage runs autonomously.
    ///
    /// Grants nothing and changes no behaviour. Its one consumer is
    /// `lev validate`, where it silences the
    /// `blocking-tool-in-autonomous-stage` lint. An autonomous stage that
    /// calls one of those tools with nobody attached parks in `WAITING_INPUT`
    /// until a person answers or the run is cancelled.
    async fn declares_blocking_tools(&self) -> bool {
        self.stage().allow_blocking_tools
    }

    /// The prompt guidance this stage declares, before the cascade.
    async fn tool_guidance(&self) -> ToolUseGuidance {
        ToolUseGuidance {
            batch_independent_calls: self.stage().batch_tool_hint.into(),
            shell_for_multi_step_work: self.stage().shell_hint.into(),
        }
    }

    /// What this stage does with each tool it names, where that differs from the
    /// blueprint's own rules. A stage may only tighten them.
    async fn tool_permissions(&self) -> Vec<ToolPermissionRule> {
        ToolPermissionRule::from_table(&self.stage().tool_permissions)
    }

    /// What each tool may be handed here, as mime patterns.
    async fn tool_accepts(&self) -> Vec<ToolAcceptRule> {
        self.stage()
            .tool_accepts
            .iter()
            .map(|(tool, patterns)| ToolAcceptRule {
                tool: tool.clone(),
                patterns: patterns.clone(),
            })
            .collect()
    }

    /// Where this stage's tool results land in its context. Null leaves the
    /// daemon's own routing.
    async fn tool_routing(&self) -> Option<ToolRouting> {
        self.stage()
            .tool_result_routing
            .as_ref()
            .map(ToolRouting::from)
    }

    /// Where the parts this stage produces are written, by mime pattern.
    async fn output_routing(&self) -> Vec<OutputRoute> {
        self.stage()
            .output_routing
            .iter()
            .map(|(pattern, region)| OutputRoute {
                pattern: pattern.clone(),
                region: region.clone(),
            })
            .collect()
    }

    /// Which regions this stage adds, hides or empties.
    async fn context(&self) -> StageContext {
        let stage = self.stage();
        StageContext {
            regions: stage
                .context_layout
                .as_ref()
                .map(|layout| {
                    layout
                        .regions
                        .iter()
                        .map(|region| region.name.clone())
                        .collect()
                })
                .unwrap_or_default(),
            hide: stage.context_hide.clone(),
            reset: stage.context_reset.clone(),
        }
    }

    /// What happens when the model answers with text before calling any tool.
    /// Null inherits the blueprint's setting.
    async fn nudge(&self) -> Option<NudgeConfig> {
        self.stage().nudge.as_ref().map(NudgeConfig::from)
    }

    /// Where this stage's tools run. Null inherits the blueprint's setting.
    async fn sandbox(&self) -> Option<SandboxConfig> {
        self.stage().sandbox.as_ref().map(SandboxConfig::from)
    }

    /// What this stage asks of the taint layer. Null inherits the blueprint's
    /// setting, which inherits the machine's.
    async fn security(&self) -> Option<BlueprintSecurity> {
        self.stage().security.as_ref().map(BlueprintSecurity::from)
    }

    /// The scripts this stage runs at points in its own lifecycle. Null when it
    /// declares none, which costs nothing at run time.
    async fn hooks(&self) -> Option<StageHooks> {
        let hooks = &self.stage().hooks;
        match hooks.is_empty() {
            true => None,
            false => Some(StageHooks::from(hooks)),
        }
    }

    /// The checkpoints this stage raises, where the run waits for a person.
    /// Empty for every mode but `INTERACTIVE_POINTS`.
    async fn interaction_points(&self) -> Vec<InteractionPoint> {
        match &self.stage().mode {
            leviath_core::blueprint::StageMode::InteractivePoints { points } => {
                points.iter().map(InteractionPoint::from).collect()
            }
            _ => Vec::new(),
        }
    }

    /// What this stage splits into, for a `FAN_OUT` stage. Null for every other
    /// mode.
    async fn fan_out(&self) -> Option<FanOut> {
        match &self.stage().mode {
            leviath_core::blueprint::StageMode::FanOut { config } => Some(FanOut::from(config)),
            _ => None,
        }
    }

    /// What this stage's settings resolve to against this machine's config, as
    /// they would for a run spawned now.
    ///
    /// The fields above are what the author declared, which is a different
    /// question: a stage that declares nothing still runs with something.
    async fn effective(&self, ctx: &async_graphql::Context<'_>) -> EffectiveStageSettings {
        let state = ctx.data_unchecked::<crate::commands::serve::types::AppState>();
        let config = state.current_config();
        let stage = self.stage();
        let reviewed = matches!(
            &stage.mode,
            leviath_core::blueprint::StageMode::InteractivePoints { points } if !points.is_empty()
        );
        let nudge = leviath_core::resolve_nudge(
            Some(&config.nudge),
            self.blueprint.nudge.as_ref(),
            stage.nudge.as_ref(),
            reviewed,
        );
        EffectiveStageSettings {
            includes_batch_hint: leviath_core::taint::resolve_batch_tool_hint(
                config.batch_tool_hint,
                self.blueprint.batch_tool_hint,
                stage.batch_tool_hint,
            ),
            shell_hint_eligible: leviath_core::taint::resolve_shell_hint(
                config.shell_hint,
                self.blueprint.shell_hint,
                stage.shell_hint,
            ),
            nudge: EffectiveNudge {
                nudges: nudge.enabled,
                max: count(nudge.max),
                text: nudge.text,
            },
            sandbox: SandboxConfig::from(&leviath_core::sandbox::resolve_sandbox(
                config.sandbox.as_ref(),
                self.blueprint.sandbox.as_ref(),
                stage.sandbox.as_ref(),
            )),
            tracks_taint: leviath_core::taint::resolve_taint_enabled(
                config.taint_tracking,
                self.blueprint.security.as_ref(),
                stage.security.as_ref(),
            ),
        }
    }

    /// Outgoing edges. An empty list marks a terminal stage.
    async fn transitions(&self) -> Vec<TransitionEdge> {
        let mut edges: Vec<TransitionEdge> = self
            .stage()
            .transitions
            .iter()
            .flatten()
            .map(|(target, edge)| {
                let mut mapped = TransitionEdge::from_core(edge);
                // The manifest keys these by target and the parser fills the
                // edge's own copy from that key, so the map key is the
                // authority when an older record carries an empty one.
                mapped.target = target.clone();
                mapped
            })
            .collect();
        // The manifest holds these in a map, so a listing sorted by target is
        // the only order two identical requests can both produce.
        edges.sort_by(|a, b| a.target.cmp(&b.target));
        edges
    }
}

impl Stage {
    /// The stage this object stands for.
    fn stage(&self) -> &leviath_core::blueprint::Stage {
        &self.blueprint.stages[self.at]
    }
}

/// What the nudge resolves to for this stage.
#[derive(Debug, SimpleObject)]
pub(crate) struct EffectiveNudge {
    /// Whether a text-only answer sends the stage back round. Off by default for
    /// a stage with checkpoints, whose text is its work product.
    pub(crate) nudges: bool,
    /// How many text-only answers are nudged before the text is taken as final.
    pub(crate) max: i32,
    /// The nudge itself, before `{stage}` and `{regions}` are filled in.
    pub(crate) text: String,
}

/// What this stage's settings resolve to.
///
/// The cascade is stage over blueprint over the machine's config, each field on
/// its own, and this is the answer. Every name says what true means, because a
/// bare `enabled` beside three levels of inheritance reads as any of them.
///
/// This is what a run spawned **now** would get. The daemon does not record the
/// resolution a past run was spawned with, so this is not a claim about a run
/// that has already started: for that, read the run's own snapshot and the
/// config beside it.
#[derive(Debug, SimpleObject)]
pub(crate) struct EffectiveStageSettings {
    /// Whether the system prompt tells the model it may send independent tool
    /// calls together.
    pub(crate) includes_batch_hint: bool,
    /// Whether the platform's shell guidance is eligible for the system prompt.
    /// Eligible rather than present: it is emitted only where this host has
    /// guidance worth giving and the stage offers a shell tool.
    pub(crate) shell_hint_eligible: bool,
    /// What happens when the model answers with text before calling any tool.
    pub(crate) nudge: EffectiveNudge,
    /// Where this stage's tools run.
    pub(crate) sandbox: SandboxConfig,
    /// Whether content that came from outside is followed through the run, so a
    /// tool call carrying it can be held. A manifest can turn this on and cannot
    /// turn it off.
    pub(crate) tracks_taint: bool,
}
