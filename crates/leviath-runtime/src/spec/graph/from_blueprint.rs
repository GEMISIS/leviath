//! Reading a parsed [`Blueprint`] as a [`RunGraph`].
//!
//! A blueprint keeps names as plain text and some settings in open maps. This
//! checks each name, types each setting, and turns every caller-input region
//! seed into a declared `text` input bound to that region. A blueprint that
//! parsed can still fail here, and each failure is reported with the path of
//! the field that caused it.

use std::collections::BTreeMap;
use std::str::FromStr;

use leviath_core::JsonDoc;
use leviath_core::policy::ToolPolicy;
use leviath_core::region::RegionKind as CoreKind;

use super::*;
use crate::spec::blueprint::{self as bp, Blueprint};
use crate::spec::inputs::{InputDecl, InputSlot, InputType, RegionBinding};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::layout::{BudgetSpec, ContextLayout, RegionSeed};
use crate::spec::names::{
    BlueprintName, BlueprintRef, EdgeName, InputName, McpServerName, MimePattern, ModelId,
    ModelRef, NameError, ProviderName, RegionName, StageName, ToolName, WorkdirPath,
};

impl RunGraph {
    /// Read the two tables of a blueprint manifest that its parsed form does
    /// not carry, `[[mcp_servers]]` and `[tool_script_permissions]`, into
    /// this graph. `manifest` is the text [`RunGraph::from_blueprint`]'s
    /// blueprint was parsed from. Every entry that does not fit is reported,
    /// each at its own path.
    pub fn read_manifest_tables(&mut self, manifest: &str) -> Result<(), SpawnIssues> {
        let mut issues = SpawnIssues::new();
        let table: toml::Table = match toml::from_str(manifest) {
            Ok(table) => table,
            Err(e) => return Err(SpawnIssue::new(at(), IssueCode::Invalid, e.to_string()).into()),
        };
        if let Some(value) = table.get("mcp_servers") {
            let servers: Result<Vec<McpServerDef>, _> = value.clone().try_into();
            match servers {
                Ok(servers) => self.mcp_servers = servers,
                Err(e) => issues.push(SpawnIssue::new(
                    at().field("mcp_servers"),
                    IssueCode::Invalid,
                    e.to_string(),
                )),
            }
        }
        if let Some(value) = table.get("tool_script_permissions") {
            let perms: Result<ScriptPermissionsDef, _> = value.clone().try_into();
            match perms {
                Ok(perms) => self.script_permissions = perms,
                Err(e) => issues.push(SpawnIssue::new(
                    at().field("script_permissions"),
                    IssueCode::Invalid,
                    e.to_string(),
                )),
            }
        }
        issues.into_result(())
    }

    /// Read a parsed blueprint as a run graph, reporting every field that
    /// does not fit.
    pub fn from_blueprint(blueprint: &Blueprint) -> Result<RunGraph, SpawnIssues> {
        let mut c = Conv {
            issues: SpawnIssues::new(),
            inputs: BTreeMap::new(),
        };
        let graph = c.graph(blueprint);
        let mut graph = graph;
        graph.inputs = c.inputs.into_values().collect();
        c.issues.into_result(graph)
    }
}

struct Conv {
    issues: SpawnIssues,
    /// Caller inputs found in region seeds, by input name.
    inputs: BTreeMap<String, InputDecl>,
}

fn at() -> SpecPath {
    SpecPath::root()
}

impl Conv {
    fn bad(&mut self, path: SpecPath, err: impl ToString) {
        self.issues
            .push(SpawnIssue::new(path, IssueCode::Invalid, err.to_string()));
    }

    fn name<T: FromStr<Err = NameError>>(&mut self, path: SpecPath, text: &str) -> Option<T> {
        match text.parse::<T>() {
            Ok(v) => Some(v),
            Err(e) => {
                self.bad(path, e);
                None
            }
        }
    }

    fn names<T: FromStr<Err = NameError>>(&mut self, path: SpecPath, texts: &[String]) -> Vec<T> {
        texts
            .iter()
            .enumerate()
            .filter_map(|(i, t)| self.name(path.index(i), t))
            .collect()
    }

    fn small(&mut self, path: SpecPath, n: usize) -> u32 {
        match u32::try_from(n) {
            Ok(v) => v,
            Err(_) => {
                self.bad(path, format!("{n} is too large"));
                u32::MAX
            }
        }
    }

    fn opt_small(&mut self, path: SpecPath, n: Option<usize>) -> Option<u32> {
        n.map(|n| self.small(path, n))
    }

    fn policy(&mut self, path: SpecPath, text: &str) -> Option<ToolPolicy> {
        match text {
            "allow" => Some(ToolPolicy::Allow),
            "ask" => Some(ToolPolicy::Ask),
            "deny" => Some(ToolPolicy::Deny),
            other => {
                self.issues.push(
                    SpawnIssue::new(
                        path,
                        IssueCode::Invalid,
                        format!("\"{other}\" is not a tool policy"),
                    )
                    .known(["allow", "ask", "deny"]),
                );
                None
            }
        }
    }

    fn permissions<'a>(
        &mut self,
        path: SpecPath,
        entries: impl Iterator<Item = (&'a String, &'a String)>,
    ) -> BTreeMap<ToolName, ToolPolicy> {
        let mut out = BTreeMap::new();
        for (tool, policy) in entries {
            let p = path.key(tool);
            if let (Some(t), Some(pol)) = (
                self.name::<ToolName>(p.clone(), tool),
                self.policy(p, policy),
            ) {
                out.insert(t, pol);
            }
        }
        out
    }

    fn graph(&mut self, b: &Blueprint) -> RunGraph {
        let tool_permissions = self.permissions(
            at().field("tool_permissions"),
            b.agent_tool_permissions()
                .iter()
                .collect::<BTreeMap<_, _>>()
                .into_iter(),
        );
        let entry = b
            .entry_stage
            .as_ref()
            .and_then(|e| self.name(at().field("entry"), e));
        let stages: Vec<StageDef> = b.stages.iter().map(|s| self.stage(s)).collect();
        let edges = b
            .stages
            .iter()
            .zip(&stages)
            .enumerate()
            .flat_map(|(i, (s, def))| self.edges(s, def, stages.get(i + 1)))
            .collect();
        let layout = self.layout(&b.context_layout, &at().field("layout"));
        RunGraph {
            title: Some(b.name.clone()),
            description: (!b.description.is_empty()).then(|| b.description.clone()),
            entry,
            stages,
            edges,
            layout,
            inputs: Vec::new(),
            output: b
                .output
                .as_ref()
                .map(|o| self.output(o, &at().field("output"))),
            compaction: b.compaction_config.as_ref().map(|c| self.compaction(c)),
            max_child_depth: b
                .max_child_depth
                .map(|d| u8::try_from(d).unwrap_or(u8::MAX)),
            taint_tracking: b.security.as_ref().map(|s| s.taint_tracking),
            tool_permissions,
            sandbox: b.sandbox.as_ref().map(sandbox),
            read_paths: b
                .read_paths
                .as_ref()
                .map(|r| r.allow.clone())
                .unwrap_or_default(),
            safe_commands: match &b.safe_commands {
                Some(s) => SafeCommandsDef {
                    tools: self.names(at().field("safe_commands").field("tools"), &s.tools),
                    shell: s.shell.clone(),
                },
                None => SafeCommandsDef::default(),
            },
            batch_tool_hint: b.batch_tool_hint,
            shell_hint: b.shell_hint,
            nudge: b.nudge.as_ref().map(|n| self.nudge(n, at().field("nudge"))),
            repetition: b.repetition_detection.as_ref().map(|r| RepetitionDef {
                enabled: r.enabled,
                max_repeat_calls: self.opt_small(at().field("repetition"), r.max_repeat_calls),
                max_readonly_streak: self
                    .opt_small(at().field("repetition"), r.max_readonly_streak),
            }),
            file_tracking: b.file_tracking.as_ref().and_then(|f| {
                let p = at().field("file_tracking");
                Some(FileTrackingDef {
                    region: self.name(p.field("region"), &f.region)?,
                    track_reads: f.track_reads,
                    track_writes: f.track_writes,
                    max_file_tokens: self.opt_small(p.field("max_file_tokens"), f.max_file_tokens),
                })
            }),
            tool_rescan: match b.tool_rescan {
                bp::ToolRescan::AtSpawn => ToolRescan::AtSpawn,
                bp::ToolRescan::AfterWrites => ToolRescan::AfterWrites,
                bp::ToolRescan::BeforeDispatch => ToolRescan::BeforeDispatch,
            },
            transforms: b
                .transforms
                .iter()
                .enumerate()
                .filter_map(|(i, t)| self.transform(t, at().field("transforms").index(i)))
                .collect(),
            mime_types: self.mime_rows(&b.mime_types),
            dependencies: b
                .dependencies
                .iter()
                .enumerate()
                .filter_map(|(i, d)| self.dependency(d, at().field("dependencies").index(i)))
                .collect(),
            mcp_servers: Vec::new(),
            script_permissions: ScriptPermissionsDef::default(),
        }
    }

    fn stage(&mut self, s: &bp::Stage) -> StageDef {
        let p = at().field("stages").key(&s.name);
        let name = self
            .name(p.field("name"), &s.name)
            .unwrap_or_else(placeholder_stage);
        let tools = s
            .available_tools
            .iter()
            .enumerate()
            .filter_map(|(i, t)| match bp::ToolGroup::parse(t) {
                Some(g) => Some(ToolSelector::Group(group(g))),
                None => self
                    .name(p.field("tools").index(i), t)
                    .map(ToolSelector::Tool),
            })
            .collect();
        StageDef {
            name,
            description: s.description.clone(),
            system_prompt: s
                .config
                .get("system_prompt")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            model: self.model(&s.model, &p.field("model")),
            tools,
            required_tools: self.names(p.field("required_tools"), &s.required_tools),
            connectors: self.names::<McpServerName>(p.field("connectors"), &s.available_connectors),
            max_iterations: self.opt_small(p.field("max_iterations"), s.max_iterations),
            mode: self.mode(&s.mode, &p.field("mode")),
            layout: s
                .context_layout
                .as_ref()
                .map(|l| self.layout(l, &p.field("layout"))),
            hide: self.names(p.field("hide"), &s.context_hide),
            reset: self.names(p.field("reset"), &s.context_reset),
            tool_permissions: self.permissions(
                p.field("tool_permissions"),
                s.tool_permissions
                    .iter()
                    .collect::<BTreeMap<_, _>>()
                    .into_iter(),
            ),
            requires_children: s.requires_children,
            max_revisits: self.opt_small(p.field("max_revisits"), s.max_revisits),
            transition_prompt: s.transition_prompt.clone(),
            accepts_messages: s.accepts_messages,
            // Ending the run instead is offered only where the model picks
            // among declared edges; a stage that declares none falls through
            // or ends without being asked.
            allow_complete: s.allow_complete && s.transitions.is_some(),
            allow_as_worker: s.allow_as_worker,
            allow_blocking_tools: s.allow_blocking_tools,
            taint_tracking: s.security.as_ref().map(|c| c.taint_tracking),
            batch_tool_hint: s.batch_tool_hint,
            shell_hint: s.shell_hint,
            nudge: s.nudge.as_ref().map(|n| self.nudge(n, p.field("nudge"))),
            sandbox: s.sandbox.as_ref().map(sandbox),
            tool_routing: s
                .tool_result_routing
                .as_ref()
                .and_then(|r| self.routing(r, &p.field("tool_routing"))),
            output_routing: s
                .output_routing
                .iter()
                .filter_map(|(k, v)| {
                    Some((k.clone(), self.name(p.field("output_routing").key(k), v)?))
                })
                .collect(),
            output: s
                .output
                .as_ref()
                .map(|o| self.output(o, &p.field("output"))),
            require_output: s.require_output,
            input_accepts: self.names::<MimePattern>(p.field("input_accepts"), &s.input_accepts),
            input_as_text: self.names::<MimePattern>(p.field("input_as_text"), &s.input_as_text),
            tool_accepts: s
                .tool_accepts
                .iter()
                .filter_map(|(k, v)| {
                    let kp = p.field("tool_accepts").key(k);
                    Some((self.name(kp.clone(), k)?, self.names::<MimePattern>(kp, v)))
                })
                .collect(),
            hooks: hooks(&s.hooks),
        }
    }

    fn model(&mut self, m: &bp::ModelConfig, p: &SpecPath) -> ModelChoice {
        let models = m
            .models
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let ep = p.field("models").index(i);
                let provider = match e.provider.is_empty() {
                    true => None,
                    false => Some(self.name::<ProviderName>(ep.field("provider"), &e.provider)?),
                };
                Some(ModelRef {
                    provider,
                    model: self.name::<ModelId>(ep.field("model"), &e.model)?,
                })
            })
            .collect();
        let mut extra = BTreeMap::new();
        let mut keys: Vec<&String> = m.parameters.keys().collect();
        keys.sort();
        for key in keys {
            if key == "temperature" || key == "max_output_tokens" {
                continue;
            }
            match scalar(&m.parameters[key]) {
                Some(v) => {
                    extra.insert(key.clone(), v);
                }
                None => self.bad(
                    p.field("params").key(key),
                    "a model parameter is a boolean, a number, text, or a list of text",
                ),
            }
        }
        let max_output_tokens = match m.output_cap() {
            Ok(cap) => cap.and_then(|c| self.cap(c, &p.field("params").field("max_output_tokens"))),
            Err(e) => {
                self.bad(p.field("params").field("max_output_tokens"), e);
                None
            }
        };
        ModelChoice {
            models,
            allow_user_default: m.allow_user_default,
            params: ModelParams {
                temperature: m
                    .parameters
                    .get("temperature")
                    .and_then(|v| v.as_f64())
                    .map(|t| t as f32),
                max_output_tokens,
                extra,
            },
            request_timeout_secs: m.request_timeout_secs,
        }
    }

    fn cap(&mut self, c: bp::OutputCap, p: &SpecPath) -> Option<OutputCap> {
        Some(match c {
            bp::OutputCap::Tokens(n) => OutputCap::Tokens(self.small(p.clone(), n)),
            bp::OutputCap::WindowPercent(f) => OutputCap::WindowPercent(f),
            bp::OutputCap::RegionPercent { percent, region } => OutputCap::RegionPercent {
                percent,
                region: self.name(p.clone(), &region)?,
            },
        })
    }

    fn mode(&mut self, m: &bp::StageMode, p: &SpecPath) -> StageMode {
        match m {
            bp::StageMode::Autonomous => StageMode::Autonomous,
            bp::StageMode::Interactive => StageMode::Interactive,
            bp::StageMode::Output => StageMode::Output,
            bp::StageMode::InteractivePoints { points } => StageMode::InteractivePoints(
                points
                    .iter()
                    .enumerate()
                    .map(|(i, pt)| self.point(pt, &p.index(i)))
                    .collect(),
            ),
            bp::StageMode::FanOut { config } => match self.fan_out(config, &p.field("fan_out")) {
                Some(f) => StageMode::FanOut(f),
                None => StageMode::Autonomous,
            },
        }
    }

    fn point(&mut self, pt: &bp::InteractionPoint, p: &SpecPath) -> InteractionPointDef {
        InteractionPointDef {
            name: pt.name.clone(),
            prompt: pt.prompt.clone(),
            required: pt.required,
            unattended: match pt.unattended {
                bp::UnattendedPolicy::AutoApprove => UnattendedPoint::AutoApprove,
                bp::UnattendedPolicy::Ask => UnattendedPoint::Ask,
            },
            style: match pt.style {
                bp::InteractionStyle::FreeText => AnswerStyle::FreeText,
                bp::InteractionStyle::MultipleChoice => AnswerStyle::MultipleChoice,
                bp::InteractionStyle::Confirm => AnswerStyle::Confirm,
            },
            options: pt.options.clone(),
            directives: pt
                .directives
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            abort_options: pt.abort_options.clone(),
            edit_options: pt.edit_options.clone(),
            document_region: pt
                .document_region
                .as_ref()
                .and_then(|r| self.name(p.field("document_region"), r)),
        }
    }

    fn fan_out(&mut self, f: &bp::FanOutConfig, p: &SpecPath) -> Option<FanOutDef> {
        let worker = match (&f.worker_agent, &f.worker_stage, &f.worker_query) {
            (Some(agent), None, None) => match BlueprintRef::parse(agent) {
                Ok(r) => WorkerSource::Blueprint(r),
                Err(e) => {
                    self.bad(p.field("worker"), e);
                    return None;
                }
            },
            (None, Some(stage), None) => WorkerSource::Stage(self.name(p.field("worker"), stage)?),
            (None, None, Some(query)) => WorkerSource::Query(query.clone()),
            _ => {
                self.issues.push(SpawnIssue::new(
                    p.field("worker"),
                    IssueCode::Conflict,
                    "a fan-out names exactly one of worker_agent, worker_stage and worker_query",
                ));
                return None;
            }
        };
        Some(FanOutDef {
            worker,
            merge_stage: f
                .merge_stage
                .as_ref()
                .and_then(|m| self.name(p.field("merge_stage"), m)),
            max_workers: self.small(p.field("max_workers"), f.max_workers),
            on_worker_failure: match f.on_worker_failure {
                bp::WorkerFailurePolicy::Continue => WorkerFailure::Continue,
                bp::WorkerFailurePolicy::FailAll => WorkerFailure::FailAll,
            },
            split_prompt: f.split_prompt.clone(),
            results_region: f
                .results_region
                .as_ref()
                .and_then(|r| self.name(p.field("results_region"), r)),
            max_items: self.opt_small(p.field("max_items"), f.max_items),
            max_attempts: self.opt_small(p.field("max_attempts"), f.max_attempts),
        })
    }

    fn routing(&mut self, r: &bp::ToolResultRouting, p: &SpecPath) -> Option<ToolRoutingDef> {
        let mut tool_regions = BTreeMap::new();
        for (tool, region) in &r.tool_overrides {
            let tp = p.field("tool_regions").key(tool);
            if let (Some(t), Some(reg)) = (self.name(tp.clone(), tool), self.name(tp, region)) {
                tool_regions.insert(t, reg);
            }
        }
        let mut tool_max_result_tokens = BTreeMap::new();
        for (tool, n) in &r.tool_max_result_tokens {
            let tp = p.field("tool_max_result_tokens").key(tool);
            if let Some(t) = self.name(tp.clone(), tool) {
                let n = self.small(tp, *n);
                tool_max_result_tokens.insert(t, n);
            }
        }
        Some(ToolRoutingDef {
            default_region: self.name(p.field("default_region"), &r.default_region)?,
            tool_regions,
            keep_results: r.keep_results,
            max_result_tokens: self.opt_small(p.field("max_result_tokens"), r.max_result_tokens),
            tool_max_result_tokens,
        })
    }

    /// The edges leaving a stage. A stage with no `transitions` table goes on
    /// to `next`, the stage after it, along an [`FALL_THROUGH_EDGE`]; one
    /// with an empty table, or the last stage, has none and ends the run.
    fn edges(&mut self, s: &bp::Stage, def: &StageDef, next: Option<&StageDef>) -> Vec<EdgeDef> {
        let Some(transitions) = &s.transitions else {
            return next
                .map(|to| EdgeDef {
                    name: EdgeName::new(FALL_THROUGH_EDGE).expect("a valid edge name"),
                    from: def.name.clone(),
                    to: to.name.clone(),
                    when: EdgeCondition::Always,
                    hint: None,
                    carry: EdgeCarry::Direct,
                    gate: None,
                    stuck: None,
                })
                .into_iter()
                .collect();
        };
        let mut names: Vec<&String> = transitions.keys().collect();
        names.sort();
        names
            .into_iter()
            .filter_map(|edge_name| {
                let e = &transitions[edge_name];
                let p = at()
                    .field("stages")
                    .key(&s.name)
                    .field("transitions")
                    .key(edge_name);
                Some(EdgeDef {
                    name: self.name::<EdgeName>(p.clone(), edge_name)?,
                    from: self.name::<StageName>(p.clone(), &s.name)?,
                    to: self.name::<StageName>(p.field("target"), &e.target)?,
                    when: match e.condition {
                        bp::TransitionCondition::Always => EdgeCondition::Always,
                        bp::TransitionCondition::Error => EdgeCondition::Error,
                        bp::TransitionCondition::MaxIterations => EdgeCondition::MaxIterations,
                        bp::TransitionCondition::LlmChoice => EdgeCondition::LlmChoice,
                        bp::TransitionCondition::DeadEnd => EdgeCondition::DeadEnd,
                        bp::TransitionCondition::Stuck => EdgeCondition::Stuck,
                    },
                    hint: e.hint.clone(),
                    carry: self.carry(&e.transform, &p.field("transform")),
                    gate: e.gate.as_ref().map(|g| self.gate(g, &p.field("gate"))),
                    stuck: e.stuck.as_ref().map(|st| StuckDef {
                        after_iterations: self.opt_small(p.field("stuck"), st.after_iterations),
                        after_minutes: self.opt_small(p.field("stuck"), st.after_minutes),
                        after_same_file_edits: self
                            .opt_small(p.field("stuck"), st.after_same_file_edits),
                        after_tool_calls: self.opt_small(p.field("stuck"), st.after_tool_calls),
                    }),
                })
            })
            .collect()
    }

    fn carry(&mut self, t: &bp::EdgeTransform, p: &SpecPath) -> EdgeCarry {
        match t {
            bp::EdgeTransform::Direct => EdgeCarry::Direct,
            bp::EdgeTransform::Clear => EdgeCarry::Clear,
            bp::EdgeTransform::Compact { prompt } => EdgeCarry::Compact {
                prompt: prompt.clone(),
            },
            bp::EdgeTransform::Custom {
                carry,
                compact,
                clear,
                compact_prompt,
            } => EdgeCarry::Custom {
                carry: self.names(p.field("carry"), carry),
                compact: self.names(p.field("compact"), compact),
                clear: self.names(p.field("clear"), clear),
                compact_prompt: compact_prompt.clone(),
            },
        }
    }

    fn gate(&mut self, g: &bp::TransitionGate, p: &SpecPath) -> GateDef {
        GateDef {
            require_modifications: g.require_modifications,
            message: g.message.clone(),
            region: g
                .region
                .as_ref()
                .and_then(|r| self.name(p.field("region"), r)),
            tools: self.names(p.field("tools"), &g.tools),
            max_attempts: self.opt_small(p.field("max_attempts"), g.max_attempts),
            require_region_updated: g
                .require_region_updated
                .as_ref()
                .and_then(|r| self.name(p.field("require_region_updated"), r)),
            require_regions: self.names(p.field("require_regions"), &g.require_regions),
            require_no_open_items: g
                .require_no_open_items
                .as_ref()
                .and_then(|r| self.name(p.field("require_no_open_items"), r)),
            require_region_entries: g.require_region_entries.as_ref().and_then(|c| {
                let cp = p.field("require_region_entries");
                Some(RegionCount {
                    region: self.name(cp.field("region"), &c.region)?,
                    at_least: self.small(cp.field("at_least"), c.at_least),
                })
            }),
        }
    }

    fn layout(&mut self, l: &ContextLayout, p: &SpecPath) -> RegionLayoutDef {
        RegionLayoutDef {
            regions: l
                .regions
                .iter()
                .enumerate()
                .filter_map(|(i, r)| self.region(r, &p.field("regions").index(i)))
                .collect(),
            total_budget_tokens: self.small(p.field("total_budget_tokens"), l.total_budget_tokens),
            eviction_order: self.names(p.field("eviction_order"), &l.eviction_order),
        }
    }

    fn region(
        &mut self,
        r: &crate::spec::layout::RegionDefinition,
        p: &SpecPath,
    ) -> Option<RegionDef> {
        let name: RegionName = self.name(p.field("name"), &r.name)?;
        let kind = match &r.kind {
            CoreKind::Pinned => RegionKind::Pinned,
            CoreKind::SlidingWindow {
                max_items,
                eviction_strategy,
            } => RegionKind::SlidingWindow {
                max_items: self.small(p.field("kind"), *max_items),
                eviction: self.eviction(eviction_strategy, &p.field("kind")),
            },
            CoreKind::Temporary => RegionKind::Temporary,
            CoreKind::Compacting { threshold_tokens } => RegionKind::Compacting {
                threshold_tokens: (*threshold_tokens != usize::MAX)
                    .then(|| self.small(p.field("kind"), *threshold_tokens)),
            },
            CoreKind::Clearable => RegionKind::Clearable,
            // A blueprint that names no source writes it as empty text.
            CoreKind::CompactHistory { source_region } => RegionKind::CompactHistory {
                source: match source_region.is_empty() {
                    true => None,
                    false => Some(self.name(p.field("kind"), source_region)?),
                },
            },
            CoreKind::HashMap { max_entries } => RegionKind::Keyed {
                max_entries: self.opt_small(p.field("kind"), *max_entries),
            },
            CoreKind::Checklist => RegionKind::Checklist,
            CoreKind::Custom { script, pinned } => RegionKind::Custom {
                code: CodeRef::File(script.clone()),
                pinned: *pinned,
            },
        };
        let budget = match &r.budget {
            BudgetSpec::Absolute(n) => Budget::Tokens(self.small(p.field("budget"), *n)),
            BudgetSpec::Percent { percent, min, max } => Budget::Percent {
                percent: *percent,
                min: self.opt_small(p.field("budget"), *min),
                max: self.opt_small(p.field("budget"), *max),
            },
        };
        let seed = r
            .seed
            .as_ref()
            .and_then(|s| self.seed(s, &name, r, &p.field("seed")));
        Some(RegionDef {
            name,
            kind,
            budget,
            compact_at: r.compact_at,
            description: r.description.clone(),
            describe_in_prompt: r.describe_in_prompt,
            required: r.required,
            required_message: r.required_message.clone(),
            summarizable: r.summarizable,
            admission: r.admission,
            volatility: r.volatility,
            seed,
            accepts: self.names(p.field("accepts"), &r.accepts),
        })
    }

    /// A caller-input seed becomes a `text` input bound to the region. Two
    /// regions fed from one caller key share one input.
    fn caller_input(
        &mut self,
        input: &str,
        region: &RegionName,
        r: &crate::spec::layout::RegionDefinition,
        p: &SpecPath,
    ) {
        let Some(name) = self.name::<InputName>(p.clone(), input) else {
            return;
        };
        let binding = InputSlot::Region(RegionBinding {
            region: region.clone(),
            template: None,
        });
        let decl = self
            .inputs
            .entry(input.to_string())
            .or_insert_with(|| InputDecl {
                name,
                ty: InputType::Text {
                    multiline: true,
                    min_len: None,
                    max_len: None,
                },
                required: false,
                default: None,
                description: r.description.clone(),
                binds: Vec::new(),
            });
        decl.required |= r.required;
        decl.binds.push(binding);
    }

    /// What fills a region at spawn. A caller-input seed fills nothing here:
    /// it becomes an input bound to the region instead.
    fn seed(
        &mut self,
        s: &RegionSeed,
        region: &RegionName,
        r: &crate::spec::layout::RegionDefinition,
        p: &SpecPath,
    ) -> Option<Seed> {
        Some(match s {
            RegionSeed::CallerInput { name: input } => {
                self.caller_input(input, region, r, p);
                return None;
            }
            RegionSeed::Glob { pattern } => Seed::Glob(pattern.clone()),
            RegionSeed::Files { paths } => Seed::Files(self.names::<WorkdirPath>(p.clone(), paths)),
            RegionSeed::Literal { text } => Seed::Literal(text.clone()),
            RegionSeed::Rhai { script } => Seed::Code(CodeRef::File(script.clone())),
            RegionSeed::Command { command } => Seed::Command(command.clone()),
            RegionSeed::Tools { calls, refresh } => Seed::Tools {
                calls: calls
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| {
                        Some(SeedToolCall {
                            tool: self.name(p.index(i), &c.name)?,
                            args: JsonDoc::new(c.args.clone()),
                        })
                    })
                    .collect(),
                refresh: match refresh {
                    crate::spec::layout::SeedRefresh::Once => SeedRefresh::Once,
                    crate::spec::layout::SeedRefresh::EachStage => SeedRefresh::EachStage,
                },
            },
        })
    }

    fn output(&mut self, o: &leviath_core::output::OutputSpec, p: &SpecPath) -> OutputDef {
        OutputDef {
            format: o.format.clone(),
            instructions: o.instructions.clone(),
            example: o.example.clone(),
            schema: o.schema.clone().map(JsonDoc::new),
            validator: o.validator.clone().map(CodeRef::File),
            on_validator_error: o.on_validator_error,
            overwrite_artifacts: o.overwrite_artifacts,
            artifacts: o
                .artifacts
                .iter()
                .enumerate()
                .filter_map(|(i, a)| {
                    Some(ArtifactDef {
                        name: a.name.clone(),
                        mime_type: self.name(p.field("artifacts").index(i), &a.mime_type)?,
                        required: a.required,
                        description: a.description.clone(),
                    })
                })
                .collect(),
        }
    }

    fn compaction(&mut self, c: &leviath_core::lifecycle::CompactionConfig) -> CompactionDef {
        let p = at().field("compaction");
        let provider = self.name::<ProviderName>(p.field("provider"), &c.provider);
        let model = self.name::<ModelId>(p.field("model"), &c.model);
        CompactionDef {
            model: ModelRef {
                provider,
                model: model.unwrap_or_else(|| ModelId::new("unknown").expect("a valid id")),
            },
            system_prompt: c.system_prompt.clone(),
            user_prompt_template: c.user_prompt_template.clone(),
            max_summary_tokens: self.small(p.field("max_summary_tokens"), c.max_summary_tokens),
            temperature: c.temperature,
        }
    }

    fn eviction(&mut self, e: &leviath_core::region::EvictionStrategy, p: &SpecPath) -> Eviction {
        use leviath_core::region::EvictionStrategy as E;
        match e {
            E::PerItem => Eviction::PerItem,
            E::Bulk { overflow } => Eviction::Bulk(self.small(p.clone(), *overflow)),
            E::Compact { compact_count } => {
                Eviction::Compact(self.small(p.clone(), *compact_count))
            }
        }
    }

    fn nudge(&mut self, n: &bp::NudgeConfig, p: SpecPath) -> NudgeDef {
        NudgeDef {
            enabled: n.enabled,
            max: self.opt_small(p, n.max),
            text: n.text.clone(),
        }
    }

    fn transform(&mut self, t: &bp::ContextTransform, p: SpecPath) -> Option<ContextTransformDef> {
        Some(ContextTransformDef {
            from: self.name::<BlueprintName>(p.field("from"), &t.from_blueprint)?,
            to: self.name::<BlueprintName>(p.field("to"), &t.to_blueprint)?,
            mappings: t
                .mappings
                .iter()
                .enumerate()
                .filter_map(|(i, m)| {
                    let mp = p.field("mappings").index(i);
                    Some(RegionMappingDef {
                        from: self.name(mp.field("from"), &m.from_region)?,
                        to: self.name(mp.field("to"), &m.to_region)?,
                        transform: match &m.transform {
                            None | Some(bp::ContentTransform::Direct) => ContentTransform::Direct,
                            Some(bp::ContentTransform::Summarize) => ContentTransform::Summarize,
                            Some(bp::ContentTransform::Extract { fields }) => {
                                ContentTransform::Extract(fields.clone())
                            }
                        },
                    })
                })
                .collect(),
        })
    }

    fn mime_rows(&mut self, table: &toml::Table) -> MimeRows {
        let mut rows = MimeRows::new();
        for (key, value) in table {
            let p = at().field("mime_types").key(key);
            let Some(pattern) = self.name::<MimePattern>(p.clone(), key) else {
                continue;
            };
            let row: leviath_core::mime::registry::MimeRow = match value.clone().try_into() {
                Ok(row) => row,
                Err(e) => {
                    self.bad(p, e);
                    continue;
                }
            };
            rows.insert(
                pattern,
                MimeRowDef {
                    family: row.family,
                    text: row.text,
                    tokens: row.tokens.map(|t| self.token_rule(t, &p)),
                    extensions: row.extensions,
                    magic: row.magic,
                    stand_in: row.stand_in,
                    check: row.check.filter(|c| !c.is_empty()).map(CodeRef::File),
                },
            );
        }
        rows
    }

    fn token_rule(&mut self, t: leviath_core::mime::TokenRule, p: &SpecPath) -> TokenRule {
        use leviath_core::mime::TokenRule as T;
        match t {
            T::PerByte(r) => TokenRule::PerByte(r),
            T::PerPixel { divisor, max } => TokenRule::PerPixel {
                divisor,
                max: self.small(p.clone(), max),
            },
            T::PerSecond(n) => TokenRule::PerSecond(n),
            T::PerPage(n) => TokenRule::PerPage(self.small(p.clone(), n)),
            T::Fixed(n) => TokenRule::Fixed(self.small(p.clone(), n)),
        }
    }

    fn dependency(&mut self, d: &bp::Dependency, p: SpecPath) -> Option<DependencyDef> {
        let needs = match &d.kind {
            bp::DependencyKind::McpServer { server, env } => Needs::McpServer {
                server: self.name(p.field("server"), server)?,
                env: env.clone(),
            },
            bp::DependencyKind::Env { var } => Needs::Env(var.clone()),
            bp::DependencyKind::Binary { command } => Needs::Binary(command.clone()),
            bp::DependencyKind::Script { check } => Needs::Check(CodeRef::File(check.clone())),
        };
        Some(DependencyDef {
            name: d.name.clone(),
            needs,
            required: d.required,
            remedy: d.remedy.clone(),
            description: d.description.clone(),
            install: d.install.as_ref().map(|i| InstallDef {
                command: i.command.clone(),
                commands: i.commands.clone(),
                script: i.script.clone().map(CodeRef::File),
                server: i.server.as_ref().map(|s| McpServerTemplate {
                    transport: s.transport.clone(),
                    command: s.command.clone(),
                    url: s.url.clone(),
                    args: s.args.clone(),
                    headers: s.headers.clone(),
                    env: s.env.clone(),
                }),
            }),
        })
    }
}

/// Stands in for a stage whose name failed its check, so the rest of the
/// stage is still checked; the failure is already recorded.
fn placeholder_stage() -> StageName {
    StageName::new("(invalid)").expect("a valid label")
}

fn group(g: bp::ToolGroup) -> ToolGroup {
    match g {
        bp::ToolGroup::All => ToolGroup::All,
        bp::ToolGroup::Builtin => ToolGroup::Builtin,
        bp::ToolGroup::Subagent => ToolGroup::Subagent,
        bp::ToolGroup::Scripts => ToolGroup::Scripts,
        bp::ToolGroup::Mcp => ToolGroup::Mcp,
    }
}

fn sandbox(s: &leviath_core::sandbox::ToolSandboxConfig) -> SandboxDef {
    SandboxDef {
        kind: s.kind,
        image: s.image.clone(),
        engine: s.engine.clone(),
        network: s.network,
        mounts: s.mounts.clone(),
        keep_warm: s.keep_warm,
        on_unavailable: s.on_unavailable,
    }
}

fn hooks(h: &bp::StageHooks) -> StageHooks {
    let code = |s: &Option<String>| s.clone().map(CodeRef::File);
    StageHooks {
        on_stage_enter: code(&h.on_stage_enter),
        on_stage_exit: code(&h.on_stage_exit),
        before_inference: code(&h.before_inference),
        after_inference: code(&h.after_inference),
        on_tool_call: code(&h.on_tool_call),
        on_completion: code(&h.on_completion),
        on_error: code(&h.on_error),
    }
}

/// A model parameter as a typed scalar, or `None` for a shape a provider
/// setting never takes (a table, a mixed list, null).
fn scalar(v: &serde_json::Value) -> Option<ParamScalar> {
    use serde_json::Value as V;
    match v {
        V::Bool(b) => Some(ParamScalar::Bool(*b)),
        V::Number(n) => match n.as_i64() {
            Some(i) => Some(ParamScalar::Int(i)),
            None => n.as_f64().map(ParamScalar::Float),
        },
        V::String(s) => Some(ParamScalar::Text(s.clone())),
        V::Array(items) => items
            .iter()
            .map(|i| i.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .map(ParamScalar::TextList),
        V::Null | V::Object(_) => None,
    }
}

#[cfg(test)]
#[path = "from_blueprint_tests.rs"]
mod tests;
