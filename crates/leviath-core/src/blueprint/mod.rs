//! Agent blueprints and stage definitions.
//!
//! A blueprint is the complete definition of an agent type, including its
//! execution stages, model selection, tool access, and context layout.
//! Blueprints are typically defined in `leviath.toml` files and can be
//! shared, installed, and versioned.

use crate::error::ValidationError;
use crate::layout::{ContextLayout, RegionSeed};
use crate::lifecycle::CompactionConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Regions every stage can see, whatever its own `[context.regions]` says.
///
/// The runtime adds the first three when a blueprint declares none, and carries
/// all four visible through a stage's layout swap: the first two hold the typed
/// tool_use/tool_result turns, an answer submitted early has to survive to the
/// end, and the last holds the instructions of the stage being entered. Mirrors
/// `context_setup::apply_layout`, which is where the rule is enforced.
const ALWAYS_VISIBLE_REGIONS: [&str; 4] = [
    "conversation",
    "tool_results",
    "final_output",
    crate::layout::STAGE_INSTRUCTIONS_REGION,
];

/// An agent blueprint - the complete definition of an agent type.
///
/// Includes stages, model selection, tools, AND context layout. A blueprint
/// defines everything needed to instantiate and run an agent with specific
/// capabilities and memory structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blueprint {
    /// Unique name for this agent type
    pub name: String,

    /// Human-readable description
    pub description: String,

    /// Execution stages (e.g., analyze → implement → review)
    pub stages: Vec<Stage>,

    /// Context window layout defining memory regions
    pub context_layout: ContextLayout,

    /// Context transforms for inter-agent communication
    pub transforms: Vec<ContextTransform>,

    /// Version of this blueprint
    pub version: String,

    /// Configuration for LLM-based compaction
    pub compaction_config: Option<CompactionConfig>,

    /// Maximum depth of the sub-agent tree (default: 3)
    pub max_child_depth: Option<usize>,

    /// Which stage to start from (default: first defined)
    pub entry_stage: Option<String>,

    /// Additional metadata
    pub metadata: HashMap<String, serde_json::Value>,

    /// Security configuration for taint tracking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<crate::taint::SecurityConfig>,

    /// Agent-level override for the batch-tool-calls system-prompt hint. `None`
    /// inherits the global config toggle; a per-stage `batch_tool_hint` overrides
    /// this. See [`crate::taint::resolve_batch_tool_hint`] for the cascade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_tool_hint: Option<bool>,

    /// Agent-level override for the platform shell hint. `None` inherits the
    /// global config toggle; a per-stage `shell_hint` overrides this. See
    /// [`crate::taint::resolve_shell_hint`] for the cascade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_hint: Option<bool>,

    /// Agent-level default for the empty-response nudge. `None` inherits the
    /// global config's `[nudge]` section; a per-stage `[stages.<name>.nudge]`
    /// overrides this. See [`resolve_nudge`] for the cascade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nudge: Option<NudgeConfig>,

    /// Repetition detection configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repetition_detection: Option<RepetitionDetectionConfig>,

    /// File tracking configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_tracking: Option<FileTrackingConfig>,

    /// Agent-level sandbox configuration for tool execution. Per-stage
    /// `[stages.<name>.sandbox]` overrides this; both cascade through
    /// [`crate::resolve_sandbox`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<crate::sandbox::ToolSandboxConfig>,

    /// Opt-in escape hatch: when `true`, the agent may add tools to
    /// its own `tools/` directory mid-run and have them re-discovered and
    /// re-advertised for its next turn. **Off by default** - tools are otherwise
    /// discovered once at spawn and an agent cannot grow its own toolchain.
    #[serde(default)]
    pub dynamic_tools: bool,

    /// Read paths this agent *declares* beyond its workdir - directories a
    /// planner-style agent needs to see, like run archives or design docs.
    /// Declaring is not granting: entries only take effect when the user's
    /// config also grants them (`[security] read_paths`,
    /// `[agent_read_paths.<name>]`, or `allow_blueprint_read_paths = true`),
    /// so an installed manifest cannot widen its own sandbox. Read-only in
    /// every case; `write_file` and `edit_file` stay confined to the workdir.
    /// Semantics live in [`crate::read_paths`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_paths: Option<ReadPathsConfig>,

    /// The `[safe_commands]` section: tools and shell command prefixes this
    /// agent would like to run without an approval prompt.
    ///
    /// Declaring is not granting, exactly as for [`Self::read_paths`]: entries
    /// take effect only when the user opts in, per agent via
    /// `[agent_safe_commands.<name>] allow_blueprint = true` or globally via
    /// `[security] allow_blueprint_safe_commands`. Otherwise any agent package
    /// could pre-approve its own shell with one TOML line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_commands: Option<SafeCommandsConfig>,

    /// Agent-level default shape for the run's final output. A per-stage
    /// `[stages.<name>.output]` narrows it, and whoever starts the run can
    /// override it again. See [`crate::output::resolve_output_spec`].
    ///
    /// `None` means this agent declares no shape, which is not the same as
    /// producing no output: a stage may still ask for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<crate::output::OutputSpec>,
}

/// The `[safe_commands]` section of a manifest.
///
/// Entry syntax is not checked here. What counts as a usable shell prefix is
/// defined by the key parser in the CLI (a program, optionally with the
/// subcommand that narrows it), which this crate does not depend on. A bad
/// entry is a lint finding and is skipped with a warning at spawn, rather than
/// a parse error - the same place the check can be written once instead of
/// twice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeCommandsConfig {
    /// Tools that need no prompt whatever their arguments.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Shell command prefixes that need no prompt: `"cargo test"`, not
    /// `"cargo test --lib"` and never `"cargo"`.
    #[serde(default)]
    pub shell: Vec<String>,
}

/// The `[read_paths]` section of a manifest: raw declared entries, compiled
/// against the run's workdir and home at spawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadPathsConfig {
    /// Declared entries. Each may be:
    /// - an exact path, granting its subtree: `"~/.leviath/runs"` or
    ///   `"../shared-docs"` (relative to the run's workdir)
    /// - a glob: `"glob:~/.leviath/runs/**"`
    /// - a regex, auto-anchored: `"regex:/data/design-docs/.*"`
    ///
    /// Patterns are written with `/` separators on every OS and match the
    /// symlink-resolved real path.
    #[serde(default)]
    pub allow: Vec<String>,
}

impl Blueprint {
    /// Create a new blueprint with the specified configuration.
    pub fn new(
        name: String,
        description: String,
        stages: Vec<Stage>,
        context_layout: ContextLayout,
    ) -> Self {
        Self {
            name,
            description,
            stages,
            context_layout,
            transforms: Vec::new(),
            version: "0.1.0".to_string(),
            compaction_config: None,
            max_child_depth: None,
            entry_stage: None,
            metadata: HashMap::new(),
            security: None,
            batch_tool_hint: None,
            shell_hint: None,
            nudge: None,
            repetition_detection: None,
            file_tracking: None,
            sandbox: None,
            dynamic_tools: false,
            read_paths: None,
            safe_commands: None,
            output: None,
        }
    }

    /// Whether any region is seeded from the caller's `task`.
    ///
    /// The blueprint's answer to "do you take a task?", which is a different
    /// question from whether one was supplied. An agent driven by named regions
    /// (`reviewer` takes `--diff` and `--criteria`) answers no, and handing it a
    /// task would put that text nowhere at all - so both the CLI, before it asks
    /// for one, and the daemon, before it spawns, ask this first.
    pub fn accepts_task(&self) -> bool {
        self.context_layout
            .regions
            .iter()
            .any(|r| matches!(&r.seed, Some(RegionSeed::CallerInput { name }) if name == "task"))
    }

    /// The caller input keys this blueprint does read, in declaration order.
    ///
    /// Used to turn "that agent takes no task" into a message naming what it
    /// takes instead, which is the difference between a dead end and a fix.
    pub fn caller_inputs(&self) -> Vec<&str> {
        self.context_layout
            .regions
            .iter()
            .filter_map(|r| match &r.seed {
                Some(RegionSeed::CallerInput { name }) => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Why a task cannot be given to this blueprint, phrased for the user.
    ///
    /// One message rather than two, because the CLI refuses before it asks for a
    /// task and the daemon refuses before it spawns, and a user who hit one and
    /// then the other should not be told two different things.
    pub fn task_refusal(&self) -> String {
        let inputs = self.caller_inputs();
        let takes = match inputs.is_empty() {
            true => "it takes no caller input at all".to_string(),
            false => format!("it takes: {}", inputs.join(", ")),
        };
        format!(
            "agent '{}' was given a task but declares no region to put it in, so the task \
             would be ignored - {takes}. Add a region seeded from the task, for example:\n\
             [context.regions]\ntask = {{ kind = \"pinned\", max_tokens = 2000, \
             required = true, seed = \"task\" }}",
            self.name,
        )
    }

    /// Agent-level tool permissions, keyed by tool name.
    ///
    /// The manifest parser records a top-level `[tool_permissions]` block as
    /// `tool_perm:<tool>` → policy-string entries in [`Self::metadata`]. This
    /// projects them back into a tool-keyed map for the runtime's agent-level
    /// permission layer. Non-`tool_perm:` keys and non-string values are ignored.
    pub fn agent_tool_permissions(&self) -> HashMap<String, String> {
        self.metadata
            .iter()
            .filter_map(|(k, v)| {
                Some((
                    k.strip_prefix("tool_perm:")?.to_string(),
                    v.as_str()?.to_string(),
                ))
            })
            .collect()
    }

    /// Add context transforms to this blueprint.
    pub fn with_transforms(mut self, transforms: Vec<ContextTransform>) -> Self {
        self.transforms = transforms;
        self
    }

    /// Set the version of this blueprint.
    pub fn with_version(mut self, version: String) -> Self {
        self.version = version;
        self
    }

    /// Validate that the blueprint is well-formed.
    pub fn validate(&self) -> std::result::Result<(), ValidationError> {
        // Validate context layout
        self.context_layout.validate()?;

        // Check that all stages have valid configurations
        for stage in &self.stages {
            stage.validate()?;
        }

        // Validate transforms reference real regions
        for transform in &self.transforms {
            transform.validate(&self.context_layout)?;
        }

        // Graph validation
        self.validate_graph()?;

        self.validate_region_references()?;

        Ok(())
    }

    /// Every region a stage can name, anywhere in this blueprint.
    ///
    /// The union of the global layout, every stage's own layout, and the three
    /// the runtime adds if nobody declared them. It is a union rather than the
    /// per-stage set on purpose: a stage that omits a region from its
    /// `[context.regions]` hides it, it does not destroy it, so naming a region
    /// another stage declared is legitimate. Only a name that exists nowhere is
    /// a typo.
    fn known_region_names(&self) -> std::collections::HashSet<&str> {
        let mut names: std::collections::HashSet<&str> = self
            .context_layout
            .regions
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        for stage in &self.stages {
            if let Some(layout) = &stage.context_layout {
                names.extend(layout.regions.iter().map(|r| r.name.as_str()));
            }
        }
        // Added by `setup_context_window` when a blueprint does not declare
        // them, so they are always addressable.
        names.extend(ALWAYS_VISIBLE_REGIONS);
        names
    }

    /// The regions `stage` can actually see while it runs.
    ///
    /// Its own `[context.regions]` when it declares one, the blueprint's
    /// otherwise, plus the regions the runtime carries visible whatever a stage
    /// says. Narrower than [`known_region_names`](Self::known_region_names),
    /// which asks only whether a name exists somewhere - the difference is the
    /// whole of #370: a region another stage declares exists, and is still not
    /// readable from here.
    fn regions_visible_to<'a>(&'a self, stage: &'a Stage) -> std::collections::HashSet<&'a str> {
        let layout = stage
            .context_layout
            .as_ref()
            .unwrap_or(&self.context_layout);
        let mut names: std::collections::HashSet<&str> =
            layout.regions.iter().map(|r| r.name.as_str()).collect();
        names.extend(ALWAYS_VISIBLE_REGIONS);
        names
    }

    /// Refuse a region name that exists nowhere in the blueprint.
    ///
    /// Routing and gates are addressed by name, and a name that matches nothing
    /// used to be accepted in silence: the routed tool result went to the
    /// default region and the gate held nothing back, both looking exactly like
    /// a working config (#362). A gate that silently never fires is the
    /// expensive case - it reads as the model behaving well.
    fn validate_region_references(&self) -> std::result::Result<(), ValidationError> {
        let known = self.known_region_names();
        let checklists: std::collections::HashSet<&str> = self
            .context_layout
            .regions
            .iter()
            .chain(
                self.stages
                    .iter()
                    .filter_map(|s| s.context_layout.as_ref())
                    .flat_map(|l| l.regions.iter()),
            )
            .filter(|r| matches!(r.kind, crate::RegionKind::Checklist))
            .map(|r| r.name.as_str())
            .collect();

        for stage in &self.stages {
            let bad = |message: String| ValidationError::Stage {
                stage: stage.name.clone(),
                message,
            };

            if let Some(routing) = &stage.tool_result_routing {
                // Routing is checked against what *this* stage can see, not
                // against every name in the blueprint. A stage that omits a
                // region from its own `[context.regions]` hides it, so a result
                // routed there is written somewhere the stage cannot read - and
                // the pointer left in `conversation` tells the model to go read
                // it. There is no reading of a blueprint where that was
                // intended (#370).
                let visible = self.regions_visible_to(stage);
                let dead_drop = |key: &str, region: &str| ValidationError::Stage {
                    stage: stage.name.clone(),
                    message: format!(
                        "tool_routing.{key} sends results to region '{region}', \
                             which this stage's context does not include, so it \
                             could not read them back. Add '{region}' to \
                             [stages.{}.context.regions], or route somewhere the \
                             stage can see.",
                        stage.name
                    ),
                };
                if !visible.contains(routing.default_region.as_str()) {
                    return Err(dead_drop("default_region", &routing.default_region));
                }
                for (tool, region) in &routing.tool_overrides {
                    if !visible.contains(region.as_str()) {
                        return Err(dead_drop(&format!("overrides.{tool}"), region));
                    }
                }
            }

            for edge in stage.transitions.iter().flat_map(|t| t.values()) {
                let Some(gate) = &edge.gate else { continue };
                for (key, region) in [
                    ("region", gate.region.as_ref()),
                    (
                        "require_region_updated",
                        gate.require_region_updated.as_ref(),
                    ),
                    ("require_no_open_items", gate.require_no_open_items.as_ref()),
                ] {
                    let Some(region) = region else { continue };
                    if !known.contains(region.as_str()) {
                        return Err(bad(format!(
                            "transition to '{}': gate.{key} names region \
                             '{region}', which no stage declares",
                            edge.target
                        )));
                    }
                }
                // A checklist gate counts open items, which only a checklist
                // region has. Pointed at any other kind it can only ever read
                // zero, so it would pass on the first attempt every time.
                if let Some(region) = &gate.require_no_open_items
                    && !checklists.contains(region.as_str())
                {
                    return Err(bad(format!(
                        "transition to '{}': gate.require_no_open_items names \
                         region '{region}', which is not a checklist region \
                         (set kind = \"checklist\" on it)",
                        edge.target
                    )));
                }
            }
        }
        Ok(())
    }

    /// Validate stage graph constraints.
    fn validate_graph(&self) -> std::result::Result<(), ValidationError> {
        let stage_names: std::collections::HashSet<&str> =
            self.stages.iter().map(|s| s.name.as_str()).collect();

        // Entry stage must exist if set
        if let Some(entry) = &self.entry_stage
            && !stage_names.contains(entry.as_str())
        {
            return Err(ValidationError::Graph(format!(
                "entry_stage '{}' does not match any defined stage",
                entry
            )));
        }

        // Fan-out stages reference a worker source + optional merge stage. These
        // are checked even for otherwise-linear blueprints (before the early
        // return below), since `worker_stage`/`merge_stage` name local stages.
        // `worker_agent`/`worker_query` are environment-dependent (resolved
        // against installed agents at run time), so they are not checked here.
        for stage in &self.stages {
            if let StageMode::FanOut { config } = &stage.mode {
                let sources = [
                    config.worker_agent.is_some(),
                    config.worker_stage.is_some(),
                    config.worker_query.is_some(),
                ]
                .iter()
                .filter(|&&set| set)
                .count();
                if sources != 1 {
                    return Err(ValidationError::Stage {
                        stage: stage.name.clone(),
                        message: "fan_out stage must set exactly one of worker_agent, \
                                  worker_stage, or worker_query"
                            .to_string(),
                    });
                }
                if let Some(ws) = &config.worker_stage {
                    match self.stages.iter().find(|s| &s.name == ws) {
                        None => {
                            return Err(ValidationError::Stage {
                                stage: stage.name.clone(),
                                message: format!("fan_out worker_stage '{}' does not exist", ws),
                            });
                        }
                        Some(target) if !target.allow_as_worker => {
                            return Err(ValidationError::Stage {
                                stage: stage.name.clone(),
                                message: format!(
                                    "fan_out worker_stage '{}' must set allow_as_worker = true",
                                    ws
                                ),
                            });
                        }
                        Some(_) => {}
                    }
                }
                if let Some(ms) = &config.merge_stage
                    && !stage_names.contains(ms.as_str())
                {
                    return Err(ValidationError::Stage {
                        stage: stage.name.clone(),
                        message: format!("fan_out merge_stage '{}' does not exist", ms),
                    });
                }
            }
        }

        let has_any_transitions = self.stages.iter().any(|s| s.transitions.is_some());
        if !has_any_transitions {
            // Pure linear mode - no graph validation needed
            return Ok(());
        }

        // All transition targets must exist
        for stage in &self.stages {
            if let Some(ref transitions) = stage.transitions {
                for (target_name, edge) in transitions {
                    if !stage_names.contains(target_name.as_str()) {
                        return Err(ValidationError::Transition {
                            from: stage.name.clone(),
                            to: target_name.clone(),
                            message: "target stage does not exist".to_string(),
                        });
                    }
                    // A `stuck` edge with no threshold could never fire. Caught
                    // here as well as in the manifest parser, so blueprints built
                    // programmatically (API / `lev validate`) are held to it too.
                    if edge.condition == TransitionCondition::Stuck
                        && !edge.stuck.is_some_and(|c| c.is_armed())
                    {
                        return Err(ValidationError::Transition {
                            from: stage.name.clone(),
                            to: target_name.clone(),
                            message: "condition = \"stuck\" requires at least one \
                                      stuck_after_* threshold (the edge could never fire)"
                                .to_string(),
                        });
                    }
                }

                // A `require_modifications` gate on a stage that advertises no
                // file-modifying tool can never be satisfied - it would just
                // burn the stage's re-run budget every time.
                for (target_name, edge) in transitions {
                    let Some(gate) = &edge.gate else { continue };
                    if !gate.require_modifications {
                        continue;
                    }
                    let can_modify = stage.available_tools.iter().any(|t| {
                        MODIFYING_TOOLS.contains(&t.as_str())
                            || gate.tools.iter().any(|extra| extra == t)
                    });
                    if !can_modify {
                        return Err(ValidationError::Transition {
                            from: stage.name.clone(),
                            to: target_name.clone(),
                            message: "gate requires modifications, but the stage has no \
                                      file-modifying tool in available_tools"
                                .to_string(),
                        });
                    }
                }

                // Self-loop safety: stages that transition to themselves need max_revisits
                if transitions.contains_key(&stage.name) && stage.max_revisits.is_none() {
                    return Err(ValidationError::Stage {
                        stage: stage.name.clone(),
                        message: "self-loop transition requires max_revisits".to_string(),
                    });
                }
            }
        }

        // At least one terminal path must exist (a stage with no outgoing transitions,
        // or with only conditional transitions that may not fire)
        let entry = self.resolve_entry_stage_name();
        let has_terminal = self.has_terminal_path(&entry, &mut std::collections::HashSet::new());
        if !has_terminal {
            return Err(ValidationError::Graph(
                "no terminal path exists from entry stage - agent would never complete".to_string(),
            ));
        }

        Ok(())
    }

    /// Resolve the entry stage name.
    pub fn resolve_entry_stage_name(&self) -> String {
        self.entry_stage.clone().unwrap_or_else(|| {
            self.stages
                .first()
                .map(|s| s.name.clone())
                .unwrap_or_default()
        })
    }

    /// Check if there is a terminal path reachable from `stage_name`.
    fn has_terminal_path(
        &self,
        stage_name: &str,
        visited: &mut std::collections::HashSet<String>,
    ) -> bool {
        if visited.contains(stage_name) {
            return false;
        }
        visited.insert(stage_name.to_string());

        let stage = self.stages.iter().find(|s| s.name == stage_name);
        let stage = match stage {
            Some(s) => s,
            // Unreachable via this function's only call site (`validate_graph`,
            // below): it rejects any transition target that doesn't match a
            // real stage name *before* ever calling `has_terminal_path`, and
            // `has_terminal_path` is private, so no other caller can pass in
            // an unvalidated stage name.
            None => return false,
        };

        // A fan-out stage with a merge stage hands off to it after workers
        // complete, so its terminal path runs through the merge stage.
        if let StageMode::FanOut {
            config:
                FanOutConfig {
                    merge_stage: Some(ms),
                    ..
                },
        } = &stage.mode
        {
            return self.has_terminal_path(ms, visited);
        }

        match &stage.transitions {
            None => {
                // Linear mode: check if there's a next stage by index
                let idx = self
                    .stages
                    .iter()
                    .position(|s| s.name == stage_name)
                    .unwrap_or(0);
                if idx + 1 >= self.stages.len() {
                    return true; // terminal
                }
                self.has_terminal_path(&self.stages[idx + 1].name, visited)
            }
            Some(transitions) => {
                if transitions.is_empty() {
                    return true; // terminal stage
                }
                // Check if any transition leads to a terminal
                for target in transitions.keys() {
                    if self.has_terminal_path(target, visited) {
                        return true;
                    }
                }
                // No target reaches a terminal stage. This used to fall back to
                // "all targets are exhaustible, so the stage will eventually
                // have zero available edges" and call THAT a terminal path -
                // but running out of edges mid-graph is now a run *error*
                // (StageResolution::DeadEnd in the runtime), not a completion,
                // so certifying it here validated blueprints that could never
                // finish successfully.
                false
            }
        }
    }

    /// Find a stage by name.
    pub fn find_stage(&self, name: &str) -> Option<&Stage> {
        self.stages.iter().find(|s| s.name == name)
    }
}

// Sections of the former single-file blueprint, one per concept. Glob
// re-exported so every existing `blueprint::Stage` path keeps working and the
// split stays a pure move.
mod model;
pub use model::*;
mod stage;
pub use stage::*;
mod transition;
pub use transition::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::ContextLayout;
    use crate::layout::RegionDefinition;
    use crate::region::RegionKind;

    /// Build a blueprint from a manifest, so these read as the TOML an author
    /// would actually write rather than as hand-assembled structs.
    fn bp_with_regions(regions_toml: &str) -> Blueprint {
        crate::manifest::parse_manifest(&format!(
            r#"
[agent]
name = "asked"

[stages.main]
mode = "autonomous"
model = {{ provider = "anthropic", model = "m" }}

[context.regions]
{regions_toml}
"#
        ))
        .expect("fixture parses")
    }

    #[test]
    fn a_blueprint_accepts_a_task_when_some_region_seeds_from_it() {
        // Both spellings: the explicit seed and the region named `task`, which
        // gets the same seed implicitly.
        assert!(
            bp_with_regions(r#"brief = { kind = "pinned", max_tokens = 10, seed = "task" }"#)
                .accepts_task()
        );
        assert!(bp_with_regions(r#"task = { kind = "pinned", max_tokens = 10 }"#).accepts_task());
    }

    #[test]
    fn a_blueprint_taking_other_caller_input_does_not_accept_a_task() {
        let bp = bp_with_regions(r#"diff = { kind = "pinned", max_tokens = 10, seed = "diff" }"#);
        assert!(!bp.accepts_task());
        assert_eq!(bp.caller_inputs(), ["diff"]);
    }

    #[test]
    fn the_refusal_names_what_the_agent_takes_instead() {
        let bp = bp_with_regions(
            r#"diff = { kind = "pinned", max_tokens = 10, seed = "diff" }
criteria = { kind = "pinned", max_tokens = 10, seed = "criteria" }"#,
        );
        let msg = bp.task_refusal();
        assert!(msg.contains("agent 'asked'"), "{msg}");
        assert!(msg.contains("it takes: diff, criteria"), "{msg}");
    }

    #[test]
    fn the_refusal_says_so_when_the_agent_takes_nothing() {
        let bp = bp_with_regions(r#"notes = { kind = "pinned", max_tokens = 10 }"#);
        assert!(bp.caller_inputs().is_empty());
        // Bound rather than called inside the assert message: a message
        // expression only runs when the assert fails, so it would be an
        // uncovered region on every green run.
        let msg = bp.task_refusal();
        assert!(msg.contains("it takes no caller input at all"), "{msg}");
    }

    #[test]
    fn resolve_nudge_defaults_when_nothing_is_configured() {
        // No config anywhere: on for a normal stage, off for a reviewed one,
        // with the built-in cap and text.
        let normal = resolve_nudge(None, None, None, false);
        assert!(normal.enabled);
        assert_eq!(normal.max, DEFAULT_MAX_NUDGES);
        assert_eq!(normal.text, DEFAULT_NUDGE_TEXT);
        let reviewed = resolve_nudge(None, None, None, true);
        assert!(!reviewed.enabled);
        // The other fields don't depend on review status.
        assert_eq!(reviewed.max, DEFAULT_MAX_NUDGES);
        assert_eq!(reviewed.text, DEFAULT_NUDGE_TEXT);
    }

    #[test]
    fn resolve_nudge_cascades_each_field_independently() {
        let global = NudgeConfig {
            enabled: Some(true),
            max: Some(10),
            text: Some("global".to_string()),
        };
        let agent = NudgeConfig {
            max: Some(2),
            ..Default::default()
        };
        let stage = NudgeConfig {
            text: Some("stage".to_string()),
            ..Default::default()
        };
        let resolved = resolve_nudge(Some(&global), Some(&agent), Some(&stage), false);
        // enabled from global, max from agent, text from stage.
        assert!(resolved.enabled);
        assert_eq!(resolved.max, 2);
        assert_eq!(resolved.text, "stage");
        // The stage level wins over both when it sets a field.
        let stage_all = NudgeConfig {
            enabled: Some(false),
            max: Some(0),
            text: Some("s".to_string()),
        };
        let resolved = resolve_nudge(Some(&global), Some(&agent), Some(&stage_all), false);
        assert_eq!(
            resolved,
            ResolvedNudge {
                enabled: false,
                max: 0,
                text: "s".to_string()
            }
        );
    }

    #[test]
    fn resolve_nudge_explicit_enabled_overrides_review_suppression() {
        // A reviewed stage is only *implicitly* exempt: any level that sets
        // `enabled` speaks for itself, in either direction.
        let on = NudgeConfig {
            enabled: Some(true),
            ..Default::default()
        };
        assert!(resolve_nudge(None, None, Some(&on), true).enabled);
        assert!(resolve_nudge(None, Some(&on), None, true).enabled);
        assert!(resolve_nudge(Some(&on), None, None, true).enabled);
        let off = NudgeConfig {
            enabled: Some(false),
            ..Default::default()
        };
        assert!(!resolve_nudge(None, None, Some(&off), false).enabled);
    }

    #[test]
    fn test_blueprint_creation() {
        let regions = vec![RegionDefinition::new(
            "test".to_string(),
            RegionKind::Pinned,
            5000,
        )];
        let layout = ContextLayout::new(regions, 10000);

        let stages = vec![Stage::new(
            "analyze".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        )];

        let blueprint = Blueprint::new(
            "test-agent".to_string(),
            "A test agent".to_string(),
            stages,
            layout,
        );

        assert_eq!(blueprint.name, "test-agent");
        assert_eq!(blueprint.stages.len(), 1);
    }

    #[test]
    fn test_blueprint_with_transforms_version() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let bp = Blueprint::new("t".into(), "d".into(), stages, make_layout())
            .with_transforms(vec![ContextTransform {
                from_blueprint: "a".to_string(),
                to_blueprint: "b".to_string(),
                mappings: vec![],
            }])
            .with_version("2.0.0".to_string());

        assert_eq!(bp.transforms.len(), 1);
        assert_eq!(bp.version, "2.0.0");
    }

    #[test]
    fn agent_tool_permissions_projects_only_string_tool_perm_entries() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let mut bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        // A well-formed tool_perm string entry - included.
        bp.metadata.insert(
            "tool_perm:bash".to_string(),
            serde_json::Value::String("deny".to_string()),
        );
        // A non-`tool_perm:` key - skipped (strip_prefix returns None).
        bp.metadata
            .insert("title".to_string(), serde_json::Value::String("x".into()));
        // A tool_perm key whose value isn't a string - skipped (as_str is None).
        bp.metadata
            .insert("tool_perm:weird".to_string(), serde_json::Value::Bool(true));

        let perms = bp.agent_tool_permissions();
        assert_eq!(perms.get("bash").map(String::as_str), Some("deny"));
        assert!(!perms.contains_key("title"));
        assert!(!perms.contains_key("weird"));
        assert_eq!(perms.len(), 1);
    }

    #[test]
    fn test_blueprint_validate_runs_transform_validation() {
        // A transform whose mapping targets a real region - validate() must
        // reach ContextTransform::validate() and succeed.
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let mut bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        bp.transforms.push(ContextTransform {
            from_blueprint: "a".to_string(),
            to_blueprint: "b".to_string(),
            mappings: vec![RegionMapping {
                from_region: "test".to_string(),
                to_region: "test".to_string(),
                transform: None,
            }],
        });
        assert!(bp.validate().is_ok());
    }

    #[test]
    fn test_blueprint_validate_fails_on_transform_targeting_unknown_region() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let mut bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        bp.transforms.push(ContextTransform {
            from_blueprint: "a".to_string(),
            to_blueprint: "b".to_string(),
            mappings: vec![RegionMapping {
                from_region: "test".to_string(),
                to_region: "nonexistent".to_string(),
                transform: None,
            }],
        });
        let err = bp.validate().unwrap_err();
        assert_eq!(
            err,
            ValidationError::Region {
                region: "nonexistent".to_string(),
                message: "transform target region not found in layout".to_string(),
            }
        );
    }

    #[test]
    fn test_mixed_linear_and_graph_mode_terminal_path() {
        // "plan" has explicit transitions (triggers graph-mode validation),
        // but "impl" and "review" have none - they must fall back to
        // linear (next-by-index) terminal-path resolution.
        let mut plan = Stage::new("plan".to_string(), make_model());
        let impl_stage = Stage::new("impl".to_string(), make_model());
        let review = Stage::new("review".to_string(), make_model());

        let mut transitions = HashMap::new();
        transitions.insert(
            "impl".to_string(),
            TransitionEdge {
                target: "impl".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        plan.transitions = Some(transitions);

        let bp = Blueprint::new(
            "t".into(),
            "".into(),
            vec![plan, impl_stage, review],
            make_layout(),
        );
        assert!(bp.validate().is_ok());
    }

    #[test]
    fn test_stage_validation() {
        let stage = Stage::new(
            "test".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        );
        assert!(stage.validate().is_ok());

        let empty_stage = Stage::new(
            "".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        );
        assert!(empty_stage.validate().is_err());
    }

    #[test]
    fn test_stage_validate_with_valid_context_layout_is_ok() {
        let mut stage = Stage::new("test".to_string(), make_model());
        stage.context_layout = Some(make_layout());
        assert!(stage.validate().is_ok());
    }

    #[test]
    fn test_stage_validate_with_invalid_context_layout_is_err() {
        // Duplicate region names make the layout itself invalid.
        let regions = vec![
            RegionDefinition::new("dup".to_string(), RegionKind::Pinned, 100),
            RegionDefinition::new("dup".to_string(), RegionKind::Temporary, 100),
        ];
        let mut stage = Stage::new("test".to_string(), make_model());
        stage.context_layout = Some(ContextLayout::new(regions, 200));
        assert!(stage.validate().is_err());
    }

    #[test]
    fn test_stage_with_tools_context_layout_description() {
        let stage = Stage::new("test".to_string(), make_model())
            .with_tools(vec!["read_file".to_string(), "bash".to_string()])
            .with_context_layout(make_layout())
            .with_description("does things".to_string());

        assert_eq!(stage.available_tools, vec!["read_file", "bash"]);
        assert!(stage.context_layout.is_some());
        assert_eq!(stage.description.as_deref(), Some("does things"));
    }

    #[test]
    fn test_stage_with_mode() {
        let stage = Stage::new("test".to_string(), make_model())
            .with_mode(StageMode::InteractivePoints { points: vec![] });
        assert_eq!(stage.mode, StageMode::InteractivePoints { points: vec![] });
    }

    #[test]
    fn test_stage_allow_complete_defaults_false() {
        let stage = Stage::new("review".to_string(), make_model());
        assert!(!stage.allow_complete);
    }

    #[test]
    fn test_stage_allow_complete_serde_default_when_missing() {
        // A serialized stage from before allow_complete existed must still
        // deserialize, defaulting to false.
        let json = r#"{
            "name": "review",
            "description": null,
            "model": {"provider": "anthropic", "model": "claude-sonnet-4-6", "parameters": {}},
            "available_tools": [],
            "max_iterations": null,
            "context_layout": null,
            "config": {},
            "transitions": null,
            "max_revisits": null,
            "transition_prompt": null
        }"#;
        let stage: Stage = serde_json::from_str(json).unwrap();
        assert!(!stage.allow_complete);
        assert!(stage.accepts_messages);
    }

    #[test]
    fn test_stage_allow_complete_roundtrip() {
        let mut stage = Stage::new("review".to_string(), make_model());
        stage.allow_complete = true;
        let json = serde_json::to_string(&stage).unwrap();
        let back: Stage = serde_json::from_str(&json).unwrap();
        assert!(back.allow_complete);
    }

    #[test]
    fn test_interaction_point_directives_default_empty() {
        let point = InteractionPoint {
            name: "plan_approval".to_string(),
            prompt: "Approve?".to_string(),
            required: true,
            unattended: UnattendedPolicy::AutoApprove,
            style: InteractionStyle::MultipleChoice,
            options: vec!["Approve".to_string(), "Revise".to_string()],
            directives: HashMap::new(),
            abort_options: Vec::new(),
            edit_options: Vec::new(),
            document_region: None,
        };
        assert!(point.directives.is_empty());
        assert!(point.abort_options.is_empty());
        assert!(point.edit_options.is_empty());
    }

    #[test]
    fn test_interaction_point_directives_roundtrip() {
        let mut directives = HashMap::new();
        directives.insert(
            "Revise".to_string(),
            "Ask what to change, then re-plan.".to_string(),
        );
        let point = InteractionPoint {
            name: "plan_approval".to_string(),
            prompt: "Approve?".to_string(),
            required: true,
            unattended: UnattendedPolicy::Ask,
            style: InteractionStyle::MultipleChoice,
            options: vec!["Approve".to_string(), "Revise".to_string()],
            directives,
            abort_options: vec!["Abort".to_string()],
            edit_options: vec!["Add detail".to_string()],
            document_region: Some("plan".to_string()),
        };
        let json = serde_json::to_string(&point).unwrap();
        let back: InteractionPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.directives.get("Revise").map(|s| s.as_str()),
            Some("Ask what to change, then re-plan.")
        );
        assert_eq!(back.abort_options, vec!["Abort".to_string()]);
        assert_eq!(back.edit_options, vec!["Add detail".to_string()]);
        // A point that holds for a person under `--yolo` has to survive the
        // round trip: this is what a restored run re-arms from.
        assert_eq!(back.unattended, UnattendedPolicy::Ask);
    }

    #[test]
    fn test_interaction_point_directives_serde_default_when_missing() {
        let json = r#"{
            "name": "plan_approval",
            "prompt": "Approve?",
            "required": true,
            "style": "multiple_choice",
            "options": ["Approve", "Revise"]
        }"#;
        let point: InteractionPoint = serde_json::from_str(json).unwrap();
        assert!(point.directives.is_empty());
        assert!(point.abort_options.is_empty());
    }

    #[test]
    fn test_interaction_point_followups_alias_still_deserializes() {
        // Backward compat: old serialized blueprints used "followups".
        let json = r#"{
            "name": "plan_approval",
            "prompt": "Approve?",
            "required": true,
            "style": "multiple_choice",
            "options": ["Approve", "Revise"],
            "followups": { "Revise": "What to change?" }
        }"#;
        let point: InteractionPoint = serde_json::from_str(json).unwrap();
        assert_eq!(
            point.directives.get("Revise").map(|s| s.as_str()),
            Some("What to change?")
        );
    }

    #[test]
    fn test_model_config_new_creates_single_entry() {
        let mc = ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string());
        assert_eq!(mc.models.len(), 1);
        assert_eq!(mc.models[0].provider, "anthropic");
        assert_eq!(mc.models[0].model, "claude-sonnet-4-6");
        assert!(mc.allow_user_default);
    }

    #[test]
    fn test_model_config_with_multiple_models() {
        let mc = ModelConfig {
            models: vec![
                ModelEntry::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
                ModelEntry::new("openai".to_string(), "gpt-4o".to_string()),
                ModelEntry::new("ollama".to_string(), "llama3".to_string()),
            ],
            allow_user_default: true,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        };
        assert_eq!(mc.models.len(), 3);
        assert_eq!(mc.models[0].provider, "anthropic");
        assert_eq!(mc.models[1].provider, "openai");
        assert_eq!(mc.models[2].provider, "ollama");
    }

    #[test]
    fn test_model_config_serde_roundtrip() {
        let mc = ModelConfig {
            models: vec![
                ModelEntry::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
                ModelEntry::new("openai".to_string(), "gpt-4o".to_string()),
            ],
            allow_user_default: false,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        };
        let json = serde_json::to_string(&mc).unwrap();
        let back: ModelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.models.len(), 2);
        assert_eq!(back.models[0].provider, "anthropic");
        assert_eq!(back.models[1].provider, "openai");
        assert!(!back.allow_user_default);
    }

    #[test]
    fn test_model_config_serde_defaults_when_fields_missing() {
        // Minimal JSON - models defaults to empty, allow_user_default defaults to true
        let json = r#"{"parameters": {}}"#;
        let mc: ModelConfig = serde_json::from_str(json).unwrap();
        assert!(mc.models.is_empty());
        assert!(mc.allow_user_default);
    }

    #[test]
    fn test_model_config_convenience_accessors() {
        let mc = ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string());
        assert_eq!(mc.provider(), "anthropic");
        assert_eq!(mc.model(), "claude-sonnet-4-6");
    }

    #[test]
    fn test_model_config_convenience_accessors_empty_models() {
        let mc = ModelConfig {
            models: vec![],
            allow_user_default: true,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        };
        assert_eq!(mc.provider(), "anthropic");
        assert_eq!(mc.model(), "claude-sonnet-4-6");
    }

    fn make_model() -> ModelConfig {
        ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string())
    }

    fn make_layout() -> ContextLayout {
        let regions = vec![RegionDefinition::new(
            "test".to_string(),
            RegionKind::Pinned,
            5000,
        )];
        ContextLayout::new(regions, 10000)
    }

    #[test]
    fn test_graph_validation_entry_stage_exists() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let mut bp = Blueprint::new("t".into(), "".into(), stages, make_layout());
        bp.entry_stage = Some("nonexistent".to_string());
        assert!(bp.validate().is_err());
    }

    #[test]
    fn test_graph_validation_entry_stage_valid() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let mut bp = Blueprint::new("t".into(), "".into(), stages, make_layout());
        bp.entry_stage = Some("plan".to_string());
        assert!(bp.validate().is_ok());
    }

    #[test]
    fn test_graph_validation_transition_target_missing() {
        let mut stage = Stage::new("plan".to_string(), make_model());
        let mut transitions = HashMap::new();
        transitions.insert(
            "nonexistent".to_string(),
            TransitionEdge {
                target: "nonexistent".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        stage.transitions = Some(transitions);
        let bp = Blueprint::new("t".into(), "".into(), vec![stage], make_layout());
        assert!(bp.validate().is_err());
    }

    /// A `require_modifications` gate on a stage that can't modify anything
    /// could never be satisfied - it would just burn the stage's re-run budget
    /// on every pass. Reject it at load time instead.
    #[test]
    fn test_graph_validation_modification_gate_needs_a_writing_stage() {
        let gated = |tools: &[&str], extra: &[&str]| {
            let mut stage = Stage::new("impl".to_string(), make_model());
            stage.available_tools = tools.iter().map(|t| t.to_string()).collect();
            let mut transitions = HashMap::new();
            transitions.insert(
                "review".to_string(),
                TransitionEdge {
                    target: "review".to_string(),
                    condition: TransitionCondition::Always,
                    hint: None,
                    transform: EdgeTransform::Direct,
                    stuck: None,
                    gate: Some(TransitionGate {
                        require_modifications: true,
                        tools: extra.iter().map(|t| t.to_string()).collect(),
                        ..Default::default()
                    }),
                },
            );
            stage.transitions = Some(transitions);
            Blueprint::new(
                "t".into(),
                "".into(),
                vec![stage, Stage::new("review".to_string(), make_model())],
                make_layout(),
            )
        };
        let err = gated(&["read_file"], &[]).validate().unwrap_err();
        assert!(err.to_string().contains("no file-modifying tool"));
        // A built-in write tool satisfies it...
        assert!(gated(&["read_file", "edit_file"], &[]).validate().is_ok());
        // ...as does one the gate itself declares (MCP / script toolchains).
        assert!(
            gated(&["read_file", "patch_file"], &["patch_file"])
                .validate()
                .is_ok()
        );
        // A gate that doesn't require modifications is never checked.
        let mut off = gated(&["read_file"], &[]);
        off.stages[0]
            .transitions
            .as_mut()
            .unwrap()
            .get_mut("review")
            .unwrap()
            .gate = Some(TransitionGate::default());
        assert!(off.validate().is_ok());
        // Neither is an edge with no gate at all.
        off.stages[0]
            .transitions
            .as_mut()
            .unwrap()
            .get_mut("review")
            .unwrap()
            .gate = None;
        assert!(off.validate().is_ok());
    }

    #[test]
    fn test_graph_validation_self_loop_requires_max_revisits() {
        let mut stage = Stage::new("impl".to_string(), make_model());
        let mut transitions = HashMap::new();
        transitions.insert(
            "impl".to_string(),
            TransitionEdge {
                target: "impl".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        stage.transitions = Some(transitions);
        let bp = Blueprint::new("t".into(), "".into(), vec![stage], make_layout());
        assert!(bp.validate().is_err());
    }

    #[test]
    fn test_graph_validation_self_loop_with_max_revisits_ok() {
        let mut stage = Stage::new("impl".to_string(), make_model());
        stage.max_revisits = Some(3);
        let mut transitions = HashMap::new();
        transitions.insert(
            "impl".to_string(),
            TransitionEdge {
                target: "impl".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        stage.transitions = Some(transitions);
        let bp = Blueprint::new("t".into(), "".into(), vec![stage], make_layout());
        // Must FAIL now: this used to pass on the theory that the self-loop
        // exhausts its max_revisits and "leaving zero edges" counts as
        // terminal - but running out of edges mid-graph is a run error
        // (StageResolution::DeadEnd), so a blueprint whose only ending is
        // exhaustion can never finish successfully.
        let err = bp
            .validate()
            .expect_err("an exhaustion-only graph is invalid");
        assert!(err.to_string().contains("no terminal path"), "{err}");
    }

    #[test]
    fn test_graph_validation_terminal_path_exists() {
        let mut plan = Stage::new("plan".to_string(), make_model());
        let mut review = Stage::new("review".to_string(), make_model());
        review.transitions = Some(HashMap::new()); // terminal: no outgoing

        let mut transitions = HashMap::new();
        transitions.insert(
            "review".to_string(),
            TransitionEdge {
                target: "review".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        plan.transitions = Some(transitions);

        let bp = Blueprint::new("t".into(), "".into(), vec![plan, review], make_layout());
        assert!(bp.validate().is_ok());
    }

    #[test]
    fn test_graph_no_terminal_path() {
        // Two stages that only transition to each other with no terminal
        let mut a = Stage::new("a".to_string(), make_model());
        let mut b = Stage::new("b".to_string(), make_model());

        let mut a_transitions = HashMap::new();
        a_transitions.insert(
            "b".to_string(),
            TransitionEdge {
                target: "b".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        a.transitions = Some(a_transitions);

        let mut b_transitions = HashMap::new();
        b_transitions.insert(
            "a".to_string(),
            TransitionEdge {
                target: "a".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        b.transitions = Some(b_transitions);

        let bp = Blueprint::new("t".into(), "".into(), vec![a, b], make_layout());
        assert!(bp.validate().is_err());
    }

    #[test]
    fn test_linear_stages_still_validate() {
        // No transitions set at all - pure linear mode
        let stages = vec![
            Stage::new("plan".to_string(), make_model()),
            Stage::new("impl".to_string(), make_model()),
            Stage::new("review".to_string(), make_model()),
        ];
        let bp = Blueprint::new("t".into(), "".into(), stages, make_layout());
        assert!(bp.validate().is_ok());
    }

    #[test]
    fn test_resolve_entry_stage_name() {
        let stages = vec![
            Stage::new("plan".to_string(), make_model()),
            Stage::new("impl".to_string(), make_model()),
        ];
        let mut bp = Blueprint::new("t".into(), "".into(), stages, make_layout());
        assert_eq!(bp.resolve_entry_stage_name(), "plan");

        bp.entry_stage = Some("impl".to_string());
        assert_eq!(bp.resolve_entry_stage_name(), "impl");
    }

    #[test]
    fn test_find_stage() {
        let stages = vec![
            Stage::new("plan".to_string(), make_model()),
            Stage::new("impl".to_string(), make_model()),
        ];
        let bp = Blueprint::new("t".into(), "".into(), stages, make_layout());
        assert!(bp.find_stage("plan").is_some());
        assert!(bp.find_stage("impl").is_some());
        assert!(bp.find_stage("nonexistent").is_none());
    }

    #[test]
    fn test_transition_condition_default() {
        let cond = TransitionCondition::default();
        assert_eq!(cond, TransitionCondition::Always);
    }

    #[test]
    fn test_edge_transform_default() {
        let t = EdgeTransform::default();
        assert_eq!(t, EdgeTransform::Direct);
    }

    #[test]
    fn test_stage_mode_equality() {
        assert_eq!(StageMode::Autonomous, StageMode::Autonomous);
        assert_eq!(StageMode::Interactive, StageMode::Interactive);
        assert_ne!(StageMode::Autonomous, StageMode::Interactive);
    }

    #[test]
    fn test_interaction_style_equality() {
        assert_eq!(InteractionStyle::FreeText, InteractionStyle::FreeText);
        assert_ne!(InteractionStyle::FreeText, InteractionStyle::MultipleChoice);
    }

    // ─── stuck detection (#106) ─────────────────────────────────────────────

    #[test]
    fn stuck_config_is_armed_only_when_a_threshold_is_set() {
        assert!(!StuckConfig::default().is_armed());
        for cfg in [
            StuckConfig {
                after_iterations: Some(1),
                ..Default::default()
            },
            StuckConfig {
                after_minutes: Some(1),
                ..Default::default()
            },
            StuckConfig {
                after_same_file_edits: Some(1),
                ..Default::default()
            },
            StuckConfig {
                after_tool_calls: Some(1),
                ..Default::default()
            },
        ] {
            assert!(cfg.is_armed(), "{cfg:?} should be armed");
        }
    }

    #[test]
    fn transition_condition_stuck_round_trips_as_snake_case() {
        let json = serde_json::to_string(&TransitionCondition::Stuck).unwrap();
        assert_eq!(json, "\"stuck\"");
        let back: TransitionCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(back, TransitionCondition::Stuck);
        assert_ne!(TransitionCondition::Stuck, TransitionCondition::Always);
    }

    #[test]
    fn transition_edge_stuck_round_trips_and_is_omitted_when_absent() {
        let plain = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Direct,
            gate: None,
            stuck: None,
        };
        let json = serde_json::to_string(&plain).unwrap();
        assert!(
            !json.contains("stuck"),
            "absent config must be skipped: {json}"
        );

        let armed = TransitionEdge {
            condition: TransitionCondition::Stuck,
            stuck: Some(StuckConfig {
                after_iterations: Some(20),
                after_minutes: Some(10),
                after_same_file_edits: Some(3),
                after_tool_calls: Some(60),
            }),
            ..plain
        };
        let back: TransitionEdge = serde_json::from_str(&serde_json::to_string(&armed).unwrap())
            .expect("armed edge round-trips");
        assert_eq!(back.condition, TransitionCondition::Stuck);
        assert_eq!(back.stuck, armed.stuck);
    }

    /// A blueprint built programmatically (API / `lev validate`) bypasses the
    /// manifest parser, so `validate` has to catch the dead-edge shape too.
    #[test]
    fn validate_rejects_a_stuck_edge_with_no_threshold() {
        let build = |stuck| {
            let mut a = Stage::new("a".to_string(), make_model());
            let b = Stage::new("b".to_string(), make_model());
            let mut transitions = std::collections::HashMap::new();
            transitions.insert(
                "b".to_string(),
                TransitionEdge {
                    target: "b".to_string(),
                    condition: TransitionCondition::Stuck,
                    hint: None,
                    transform: EdgeTransform::Direct,
                    gate: None,
                    stuck,
                },
            );
            a.transitions = Some(transitions);
            Blueprint::new("t".into(), "".into(), vec![a, b], make_layout())
        };

        for dead in [None, Some(StuckConfig::default())] {
            let err = build(dead)
                .validate()
                .expect_err("dead stuck edge rejected");
            assert!(
                format!("{err:?}").contains("stuck_after_"),
                "unexpected error: {err:?}"
            );
        }

        // The same graph with a real threshold is fine.
        assert!(
            build(Some(StuckConfig {
                after_iterations: Some(5),
                ..Default::default()
            }))
            .validate()
            .is_ok()
        );
    }

    /// `required_tools` keeps a blocking human tool through an unattended run.
    /// Naming one the stage can't call keeps nothing, so it is rejected rather
    /// than quietly ignored - the author meant something by writing it.
    #[test]
    fn validate_rejects_a_required_tool_the_stage_cannot_call() {
        let mut stage = Stage::new("plan".to_string(), make_model());
        stage.available_tools = vec!["read_file".to_string()];
        stage.required_tools = vec!["ask_user_text".to_string()];
        let bp = Blueprint::new("t".into(), "".into(), vec![stage], make_layout());

        let err = bp.validate().expect_err("a tool it cannot call");
        let text = format!("{err:?}");
        assert!(text.contains("ask_user_text"), "names the tool: {text}");
        assert!(text.contains("available_tools"), "says why: {text}");
    }

    #[test]
    fn validate_accepts_a_required_tool_the_stage_offers() {
        let mut stage = Stage::new("plan".to_string(), make_model());
        stage.available_tools = vec!["read_file".to_string(), "ask_user_text".to_string()];
        stage.required_tools = vec!["ask_user_text".to_string()];
        let bp = Blueprint::new("t".into(), "".into(), vec![stage], make_layout());

        bp.validate().expect("the tool is on offer");
    }

    /// A stage required to produce an output, without the tool that produces
    /// one, would spend its whole re-entry budget being nudged toward a tool it
    /// was never offered and then give up. Caught at load instead.
    #[test]
    fn validate_rejects_require_output_without_the_submit_tool() {
        let mut stage = Stage::new("summary".to_string(), make_model());
        stage.available_tools = vec!["read_file".to_string()];
        stage.require_output = true;
        let bp = Blueprint::new("t".into(), "".into(), vec![stage], make_layout());

        let err = bp.validate().expect_err("no way to submit");
        let text = format!("{err:?}");
        assert!(text.contains(SUBMIT_OUTPUT_TOOL), "names the tool: {text}");
        assert!(text.contains("require_output"), "says why: {text}");
    }

    #[test]
    fn validate_accepts_require_output_when_the_stage_can_submit() {
        let mut stage = Stage::new("summary".to_string(), make_model());
        stage.available_tools = vec![SUBMIT_OUTPUT_TOOL.to_string()];
        stage.require_output = true;
        let bp = Blueprint::new("t".into(), "".into(), vec![stage], make_layout());

        bp.validate().expect("the stage can submit");
    }

    /// Declaring a shape is not the same as demanding one, so a stage carrying
    /// only an `output` block needs no tool grant.
    #[test]
    fn validate_accepts_a_declared_shape_without_require_output() {
        let mut stage = Stage::new("summary".to_string(), make_model());
        stage.available_tools = vec!["read_file".to_string()];
        stage.output = Some(crate::output::OutputSpec {
            format: Some("a2ui".to_string()),
            ..Default::default()
        });
        let bp = Blueprint::new("t".into(), "".into(), vec![stage], make_layout());

        bp.validate().expect("declaring a shape demands nothing");
    }

    #[test]
    fn output_mode_compares_equal_only_to_itself() {
        assert_eq!(StageMode::Output, StageMode::Output);
        assert_ne!(StageMode::Output, StageMode::Autonomous);
        assert_ne!(StageMode::Autonomous, StageMode::Output);
    }

    #[test]
    fn test_transition_condition_equality() {
        assert_eq!(
            TransitionCondition::LlmChoice,
            TransitionCondition::LlmChoice
        );
        assert_ne!(TransitionCondition::Always, TransitionCondition::Error);
    }

    #[test]
    fn test_edge_transform_compact_and_custom_equality() {
        let a = EdgeTransform::Compact {
            prompt: Some("p".to_string()),
        };
        let b = EdgeTransform::Compact {
            prompt: Some("p".to_string()),
        };
        assert_eq!(a, b);

        let c1 = EdgeTransform::Custom {
            carry: vec!["a".to_string()],
            compact: vec!["b".to_string()],
            clear: vec!["c".to_string()],
            compact_prompt: Some("p".to_string()),
        };
        let c2 = c1.clone();
        assert_eq!(c1, c2);

        assert_ne!(EdgeTransform::Direct, EdgeTransform::Clear);
    }

    #[test]
    fn test_stage_accepts_messages_default_true() {
        let stage = Stage::new(
            "test".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        );
        assert!(stage.accepts_messages);
    }

    #[test]
    fn test_stage_accepts_messages_serde_roundtrip() {
        // Serialize a stage with accepts_messages = false, then deserialize
        let mut stage = Stage::new(
            "report".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-opus-4-6".to_string()),
        );
        stage.accepts_messages = false;

        let json = serde_json::to_string(&stage).expect("should serialize");
        let deserialized: Stage = serde_json::from_str(&json).expect("should deserialize");
        assert!(!deserialized.accepts_messages);
    }

    #[test]
    fn test_stage_accepts_messages_json_default() {
        // When accepts_messages is missing from JSON, it should default to true
        let json = r#"{
            "name": "analyze",
            "model": { "provider": "anthropic", "model": "claude-sonnet-4-6", "parameters": {} },
            "available_tools": [],
            "mode": "Autonomous",
            "config": {},
            "tool_permissions": {},
            "requires_children": false
        }"#;
        let stage: Stage = serde_json::from_str(json).expect("should parse");
        assert!(stage.accepts_messages);
    }

    #[test]
    fn test_has_terminal_path_unknown_stage_returns_false() {
        // `has_terminal_path` is private; this test is in the same module.
        // Calling it with a stage name that doesn't exist in the Blueprint
        // exercises the `None => return false` arm (blueprint.rs line 203).
        let stages = vec![Stage::new("start".to_string(), make_model())];
        let bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        let mut visited = std::collections::HashSet::new();
        assert!(!bp.has_terminal_path("nonexistent_stage", &mut visited));
    }

    #[test]
    fn test_blueprint_validate_fails_when_layout_has_duplicate_region() {
        let regions = vec![
            RegionDefinition::new("dup".to_string(), RegionKind::Pinned, 100),
            RegionDefinition::new("dup".to_string(), RegionKind::Temporary, 100),
        ];
        let layout = ContextLayout::new(regions, 200);
        let stages = vec![Stage::new("start".to_string(), make_model())];
        let bp = Blueprint::new("t".into(), "d".into(), stages, layout);
        assert_eq!(
            bp.validate().unwrap_err(),
            ValidationError::Region {
                region: "dup".to_string(),
                message: "duplicate region name".to_string(),
            }
        );
    }

    #[test]
    fn test_blueprint_validate_fails_when_stage_has_empty_name() {
        let stages = vec![Stage::new("".to_string(), make_model())];
        let bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        assert_eq!(
            bp.validate().unwrap_err(),
            ValidationError::Stage {
                stage: "(empty)".to_string(),
                message: "stage name cannot be empty".to_string(),
            }
        );
    }

    #[test]
    fn test_file_tracking_config_defaults() {
        let json = r#"{"region": "files"}"#;
        let config: FileTrackingConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.region, "files");
        assert!(config.track_reads);
        assert!(config.track_writes);
        assert!(config.max_file_tokens.is_none());
    }

    #[test]
    fn test_file_tracking_config_serde_roundtrip() {
        let config = FileTrackingConfig {
            region: "files".to_string(),
            track_reads: true,
            track_writes: false,
            max_file_tokens: Some(5000),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: FileTrackingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.region, "files");
        assert!(back.track_reads);
        assert!(!back.track_writes);
        assert_eq!(back.max_file_tokens, Some(5000));
    }

    #[test]
    fn test_blueprint_file_tracking_default_none() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        assert!(bp.file_tracking.is_none());
    }

    #[test]
    fn test_blueprint_file_tracking_serde_roundtrip() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let mut bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        bp.file_tracking = Some(FileTrackingConfig {
            region: "files".to_string(),
            track_reads: true,
            track_writes: true,
            max_file_tokens: Some(3000),
        });
        let json = serde_json::to_string(&bp).unwrap();
        let back: Blueprint = serde_json::from_str(&json).unwrap();
        let ft = back.file_tracking.unwrap();
        assert_eq!(ft.region, "files");
        assert_eq!(ft.max_file_tokens, Some(3000));
    }

    #[test]
    fn test_tool_result_routing_default() {
        let routing = ToolResultRouting::default();
        assert_eq!(routing.default_region, "tool_results");
        assert!(routing.persist);
        assert!(routing.tool_overrides.is_empty());
        assert!(routing.max_result_tokens.is_none());
    }

    #[test]
    fn test_stage_new_has_no_tool_result_routing() {
        let stage = Stage::new("plan".to_string(), make_model());
        assert!(stage.tool_result_routing.is_none());
    }

    #[test]
    fn test_tool_result_routing_serde_roundtrip() {
        let mut routing = ToolResultRouting {
            default_region: "custom_region".to_string(),
            persist: false,
            max_result_tokens: Some(4096),
            ..Default::default()
        };
        routing
            .tool_overrides
            .insert("read_file".to_string(), "file_reads".to_string());

        let json = serde_json::to_string(&routing).unwrap();
        let back: ToolResultRouting = serde_json::from_str(&json).unwrap();

        assert_eq!(back.default_region, "custom_region");
        assert!(!back.persist);
        assert_eq!(back.max_result_tokens, Some(4096));
        assert_eq!(
            back.tool_overrides.get("read_file").map(String::as_str),
            Some("file_reads")
        );
    }

    #[test]
    fn test_stage_with_tool_result_routing_serde_roundtrip() {
        let stages = vec![{
            let mut s = Stage::new("plan".to_string(), make_model());
            s.tool_result_routing = Some(ToolResultRouting {
                default_region: "results".to_string(),
                tool_overrides: HashMap::new(),
                persist: true,
                max_result_tokens: Some(2048),
                tool_max_result_tokens: HashMap::new(),
            });
            s
        }];
        let bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        let json = serde_json::to_string(&bp).unwrap();
        let back: Blueprint = serde_json::from_str(&json).unwrap();

        let routing = back.stages[0]
            .tool_result_routing
            .as_ref()
            .expect("tool_result_routing should be Some");
        assert_eq!(routing.default_region, "results");
        assert!(routing.persist);
        assert_eq!(routing.max_result_tokens, Some(2048));
        assert!(routing.tool_overrides.is_empty());
    }

    // ─── fan_out (StageMode::FanOut) ─────────────────────────────────────────

    fn fanout_config() -> FanOutConfig {
        FanOutConfig {
            worker_agent: None,
            worker_stage: Some("fix_worker".to_string()),
            worker_query: None,
            merge_stage: Some("merge".to_string()),
            max_workers: 3,
            on_worker_failure: WorkerFailurePolicy::Continue,
            split_prompt: "split".to_string(),
            results_region: None,
            max_items: None,
        }
    }

    /// Blueprint: fan_out stage (worker_stage=fix_worker) → merge → terminal.
    /// The merge stage carries an (empty) transitions table so the blueprint is
    /// in graph mode - this makes `validate_graph` run `has_terminal_path`,
    /// which walks the fan-out stage's merge hand-off.
    fn fanout_blueprint(worker_allowed: bool, config: FanOutConfig) -> Blueprint {
        let mut fan = Stage::new("parallel".to_string(), make_model());
        fan.mode = StageMode::FanOut { config };
        let mut worker = Stage::new("fix_worker".to_string(), make_model());
        worker.allow_as_worker = worker_allowed;
        let mut merge = Stage::new("merge".to_string(), make_model());
        merge.transitions = Some(HashMap::new()); // terminal, graph mode
        Blueprint::new(
            "t".into(),
            "d".into(),
            vec![fan, worker, merge],
            make_layout(),
        )
    }

    #[test]
    fn fanout_stagemode_partial_eq_and_default_policy() {
        let a = StageMode::FanOut {
            config: fanout_config(),
        };
        let b = StageMode::FanOut {
            config: fanout_config(),
        };
        assert_eq!(a, b);
        let mut other = fanout_config();
        other.max_workers = 99;
        assert_ne!(a, StageMode::FanOut { config: other });
        assert_ne!(a, StageMode::Autonomous);
        assert_eq!(
            WorkerFailurePolicy::default(),
            WorkerFailurePolicy::Continue
        );
    }

    #[test]
    fn fanout_config_serde_roundtrip_and_max_workers_default() {
        let toml = r#"
worker_agent = "fixer"
split_prompt = "go"
on_worker_failure = "fail_all"
"#;
        let cfg: FanOutConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.worker_agent.as_deref(), Some("fixer"));
        assert_eq!(cfg.max_workers, 4); // default
        assert_eq!(cfg.on_worker_failure, WorkerFailurePolicy::FailAll);
        // JSON round-trip preserves everything.
        let json = serde_json::to_string(&fanout_config()).unwrap();
        let back: FanOutConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, fanout_config());
    }

    #[test]
    fn fanout_validate_ok_with_allowed_worker_stage() {
        assert!(fanout_blueprint(true, fanout_config()).validate().is_ok());
    }

    #[test]
    fn fanout_validate_rejects_worker_stage_not_opted_in() {
        let err = fanout_blueprint(false, fanout_config())
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("allow_as_worker"));
    }

    #[test]
    fn fanout_validate_rejects_missing_worker_stage() {
        let mut cfg = fanout_config();
        cfg.worker_stage = Some("nope".to_string());
        let err = fanout_blueprint(true, cfg).validate().unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn fanout_validate_rejects_missing_merge_stage() {
        let mut cfg = fanout_config();
        cfg.merge_stage = Some("nomerge".to_string());
        let err = fanout_blueprint(true, cfg).validate().unwrap_err();
        assert!(err.to_string().contains("merge_stage"));
    }

    #[test]
    fn fanout_validate_rejects_wrong_worker_source_count() {
        // zero sources
        let mut cfg = fanout_config();
        cfg.worker_stage = None;
        assert!(fanout_blueprint(true, cfg).validate().is_err());
        // two sources
        let mut cfg2 = fanout_config();
        cfg2.worker_agent = Some("x".to_string()); // plus worker_stage
        assert!(fanout_blueprint(true, cfg2).validate().is_err());
    }

    #[test]
    fn fanout_terminal_path_runs_through_merge_stage() {
        // worker_agent form (no local worker_stage), merge → terminal.
        let mut cfg = fanout_config();
        cfg.worker_stage = None;
        cfg.worker_agent = Some("external".to_string());
        assert!(fanout_blueprint(false, cfg).validate().is_ok());
    }

    #[test]
    fn fanout_validate_ok_without_merge_stage() {
        // No merge stage: valid, and the fan-out stage falls through to the
        // linear next stage for its terminal path.
        let mut cfg = fanout_config();
        cfg.merge_stage = None;
        assert!(fanout_blueprint(true, cfg).validate().is_ok());
    }
}
