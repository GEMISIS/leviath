//! Structural checks on a [`RunGraph`]: every name it declares is unique and
//! every name it uses points at something declared.
//!
//! These need nothing from the machine, so they run before anything is
//! resolved. What the machine has to say (does this model exist, is this
//! MCP server configured) is the resolver's job.

use std::collections::BTreeSet;

use super::{Budget, EdgeCarry, RegionKind, RegionLayoutDef, RunGraph, StageDef, StageMode};
use crate::spec::inputs::{CheckCtx, InputDecl, InputSlot, InputType};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::RegionName;

impl RunGraph {
    /// Check the graph's own consistency. `at` is where the graph sits in the
    /// request, so paths in the issues read from the request's root.
    pub fn validate(&self, at: &SpecPath) -> Result<(), SpawnIssues> {
        let mut v = Validator {
            graph: self,
            at,
            issues: SpawnIssues::new(),
        };
        v.stages();
        v.edges();
        v.layouts();
        v.inputs();
        v.graph_refs();
        v.issues.into_result(())
    }
}

struct Validator<'a> {
    graph: &'a RunGraph,
    at: &'a SpecPath,
    issues: SpawnIssues,
}

impl<'a> Validator<'a> {
    /// The graph, borrowed for as long as the validator lives rather than
    /// through `self`, so a name read from it can sit beside a new issue.
    fn g(&self) -> &'a RunGraph {
        self.graph
    }

    fn issue(
        &mut self,
        path: SpecPath,
        code: IssueCode,
        message: impl Into<String>,
    ) -> &mut SpawnIssue {
        self.issues.push(SpawnIssue::new(path, code, message));
        self.issues.0.last_mut().expect("just pushed")
    }

    fn stage_names(&self) -> Vec<&str> {
        self.g().stages.iter().map(|s| s.name.as_str()).collect()
    }

    /// Every region any layout declares, so a reference that may meet more
    /// than one layout (an edge's carry list, an input's region) is checked
    /// against all of them.
    fn all_regions(&self) -> BTreeSet<&'a str> {
        let graph: &'a RunGraph = self.graph;
        let mut names: BTreeSet<&str> = graph
            .layout
            .regions
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        for stage in &graph.stages {
            if let Some(layout) = &stage.layout {
                names.extend(layout.regions.iter().map(|r| r.name.as_str()));
            }
        }
        names
    }

    fn need_stage(&mut self, path: SpecPath, name: &str) {
        if self.g().stage(name).is_none() {
            let known: Vec<String> = self.stage_names().iter().map(|s| s.to_string()).collect();
            self.issue(
                path,
                IssueCode::Dangling,
                format!("no stage is named \"{name}\""),
            )
            .known = known;
        }
    }

    fn need_region(&mut self, path: SpecPath, name: &RegionName, within: &BTreeSet<&str>) {
        if !within.contains(name.as_str()) {
            let known: Vec<String> = within.iter().map(|s| s.to_string()).collect();
            self.issue(
                path,
                IssueCode::Dangling,
                format!("no region is named \"{name}\""),
            )
            .known = known;
        }
    }

    fn stages(&mut self) {
        let path = self.at.field("stages");
        if self.g().stages.is_empty() {
            self.issue(
                path.clone(),
                IssueCode::Missing,
                "a run needs at least one stage",
            );
        }
        let mut seen = BTreeSet::new();
        for (i, stage) in self.g().stages.iter().enumerate() {
            if !seen.insert(stage.name.as_str()) {
                self.issue(
                    path.index(i),
                    IssueCode::Duplicate,
                    format!("stage \"{}\" is declared twice", stage.name),
                );
            }
        }
        if let Some(entry) = &self.g().entry {
            self.need_stage(self.at.field("entry"), entry.as_str());
        }
        let all = self.all_regions();
        for stage in &self.g().stages {
            self.stage(stage, &path.key(stage.name.as_str()), &all);
        }
    }

    fn stage(&mut self, stage: &StageDef, path: &SpecPath, all: &BTreeSet<&str>) {
        let own: BTreeSet<&str> = self
            .g()
            .layout_for(stage)
            .regions
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        for r in &stage.hide {
            self.need_region(path.field("hide"), r, &own);
        }
        for r in &stage.reset {
            self.need_region(path.field("reset"), r, &own);
        }
        for r in stage.output_routing.values() {
            self.need_region(path.field("output_routing"), r, &own);
        }
        if let Some(routing) = &stage.tool_routing {
            let at = path.field("tool_routing");
            self.need_region(at.field("default_region"), &routing.default_region, &own);
            for r in routing.tool_regions.values() {
                self.need_region(at.field("tool_regions"), r, &own);
            }
        }
        match &stage.mode {
            StageMode::FanOut(fan) => {
                let at = path.field("mode").field("fan_out");
                if let Some(merge) = &fan.merge_stage {
                    self.need_stage(at.field("merge_stage"), merge.as_str());
                }
                if let super::WorkerSource::Stage(worker) = &fan.worker {
                    self.need_stage(at.field("worker"), worker.as_str());
                }
                if let Some(r) = &fan.results_region {
                    self.need_region(at.field("results_region"), r, all);
                }
                if fan.max_workers == 0 {
                    self.issue(
                        at.field("max_workers"),
                        IssueCode::OutOfRange,
                        "a fan-out needs at least one worker",
                    );
                }
            }
            StageMode::InteractivePoints(points) => {
                for (i, point) in points.iter().enumerate() {
                    if let Some(r) = &point.document_region {
                        self.need_region(
                            path.field("mode").index(i).field("document_region"),
                            r,
                            &own,
                        );
                    }
                }
            }
            StageMode::Autonomous | StageMode::Interactive | StageMode::Output => {}
        }
    }

    fn edges(&mut self) {
        let path = self.at.field("edges");
        let all = self.all_regions();
        let mut seen = BTreeSet::new();
        for (i, edge) in self.g().edges.iter().enumerate() {
            let at = path.index(i);
            if !seen.insert((edge.from.as_str(), edge.name.as_str())) {
                self.issue(
                    at.field("name"),
                    IssueCode::Duplicate,
                    format!(
                        "stage \"{}\" has two edges named \"{}\"",
                        edge.from, edge.name
                    ),
                );
            }
            self.need_stage(at.field("from"), edge.from.as_str());
            self.need_stage(at.field("to"), edge.to.as_str());
            if let EdgeCarry::Custom {
                carry,
                compact,
                clear,
                ..
            } = &edge.carry
            {
                for r in carry.iter().chain(compact).chain(clear) {
                    self.need_region(at.field("carry"), r, &all);
                }
            }
            if let Some(gate) = &edge.gate {
                let g = at.field("gate");
                let named = gate
                    .region
                    .iter()
                    .chain(&gate.require_region_updated)
                    .chain(&gate.require_regions)
                    .chain(&gate.require_no_open_items)
                    .chain(gate.require_region_entries.as_ref().map(|c| &c.region));
                for r in named {
                    self.need_region(g.clone(), r, &all);
                }
            }
            if edge.when == super::EdgeCondition::Stuck && edge.stuck.is_none() {
                self.issue(
                    at.field("stuck"),
                    IssueCode::Missing,
                    "a `stuck` edge needs a `stuck` rule saying when",
                );
            }
        }
    }

    fn layouts(&mut self) {
        self.layout(&self.g().layout, &self.at.field("layout"));
        for stage in &self.g().stages {
            if let Some(layout) = &stage.layout {
                let at = self
                    .at
                    .field("stages")
                    .key(stage.name.as_str())
                    .field("layout");
                self.layout(layout, &at);
            }
        }
    }

    fn layout(&mut self, layout: &RegionLayoutDef, path: &SpecPath) {
        let names: BTreeSet<&str> = layout.regions.iter().map(|r| r.name.as_str()).collect();
        let mut seen = BTreeSet::new();
        for (i, region) in layout.regions.iter().enumerate() {
            let at = path.field("regions").index(i);
            if !seen.insert(region.name.as_str()) {
                self.issue(
                    at.clone(),
                    IssueCode::Duplicate,
                    format!("region \"{}\" is declared twice", region.name),
                );
            }
            if let RegionKind::CompactHistory {
                source: Some(source),
            } = &region.kind
            {
                self.need_region(at.field("kind").field("source"), source, &names);
            }
            if let Budget::Percent { percent, min, max } = &region.budget {
                if !(*percent > 0.0 && *percent <= 1.0) {
                    self.issue(
                        at.field("budget"),
                        IssueCode::OutOfRange,
                        format!("{percent} is not a fraction above 0 and at most 1"),
                    );
                }
                if let (Some(lo), Some(hi)) = (min, max)
                    && lo > hi
                {
                    self.issue(
                        at.field("budget"),
                        IssueCode::Conflict,
                        format!("min {lo} is above max {hi}"),
                    );
                }
            }
        }
        for r in &layout.eviction_order {
            self.need_region(path.field("eviction_order"), r, &names);
        }
    }

    fn inputs(&mut self) {
        let path = self.at.field("inputs");
        let all = self.all_regions();
        let declared: BTreeSet<&str> = self.g().inputs.iter().map(|d| d.name.as_str()).collect();
        let mut seen = BTreeSet::new();
        for decl in &self.g().inputs {
            let at = path.key(decl.name.as_str());
            if !seen.insert(decl.name.as_str()) {
                self.issue(
                    at.clone(),
                    IssueCode::Duplicate,
                    format!("input \"{}\" is declared twice", decl.name),
                );
            }
            if let Some(default) = &decl.default {
                let mut found = SpawnIssues::new();
                decl.ty.check(
                    &default.to_raw(),
                    &at.field("default"),
                    &CheckCtx::default(),
                    &mut found,
                );
                self.issues.absorb(found);
            }
            for (i, slot) in decl.binds.iter().enumerate() {
                self.slot(decl, slot, &at.field("binds").index(i), &all, &declared);
            }
        }
    }

    fn slot(
        &mut self,
        decl: &InputDecl,
        slot: &InputSlot,
        at: &SpecPath,
        all: &BTreeSet<&str>,
        declared: &BTreeSet<&str>,
    ) {
        let int_at_least_one = matches!(decl.ty, InputType::Int { min: Some(m), .. } if m >= 1);
        let (fits, wants) = match slot {
            InputSlot::Region(binding) => {
                self.need_region(at.field("region"), &binding.region, all);
                if let Some(template) = &binding.template {
                    for name in template.inputs() {
                        if !declared.contains(name.as_str()) {
                            let known: Vec<String> =
                                declared.iter().map(|s| s.to_string()).collect();
                            self.issue(
                                at.field("template"),
                                IssueCode::Dangling,
                                format!(
                                    "the template names input \"{name}\", which is not declared"
                                ),
                            )
                            .known = known;
                        }
                    }
                }
                (true, "")
            }
            InputSlot::StageModel(stage) => {
                self.need_stage(at.clone(), stage.as_str());
                (matches!(decl.ty, InputType::Model), "a `model` input")
            }
            InputSlot::StageMaxIterations(stage) => {
                self.need_stage(at.clone(), stage.as_str());
                (int_at_least_one, "an `int` input with `min` of at least 1")
            }
            InputSlot::FanOutMaxWorkers(stage) => {
                self.need_stage(at.clone(), stage.as_str());
                if let Some(s) = self.g().stage(stage.as_str())
                    && !matches!(s.mode, StageMode::FanOut(_))
                {
                    self.issue(
                        at.clone(),
                        IssueCode::Conflict,
                        format!("stage \"{stage}\" is not a fan-out stage"),
                    );
                }
                (int_at_least_one, "an `int` input with `min` of at least 1")
            }
            InputSlot::OutputFormat => (
                matches!(decl.ty, InputType::Text { .. } | InputType::Choice { .. }),
                "a `text` or `choice` input",
            ),
            InputSlot::OutputInstructions => {
                (matches!(decl.ty, InputType::Text { .. }), "a `text` input")
            }
        };
        if !fits {
            let issue = self.issue(
                at.clone(),
                IssueCode::WrongType,
                "this slot does not take this input's type",
            );
            issue.expected = Some(wants.to_string());
            issue.got = Some(decl.ty.describe());
        }
    }

    fn graph_refs(&mut self) {
        let all = self.all_regions();
        if let Some(tracking) = &self.g().file_tracking {
            self.need_region(
                self.at.field("file_tracking").field("region"),
                &tracking.region,
                &all,
            );
        }
    }
}
