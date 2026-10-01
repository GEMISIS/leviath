//! Step 10's second half: what each region holds at spawn.
//!
//! A region's starting content is put together in one order, always:
//!
//! 1. its seed's output (text, then any parts the seed produced);
//! 2. the text of each input bound to it, in the order the graph declares the
//!    inputs, rendered through the binding's template when it has one;
//! 3. the files placed in it: a `file` input's attachment, or an attachment
//!    sent straight to the region, each with its caption as text.
//!
//! Text pieces are joined with a blank line between them. A piece that is
//! only whitespace is left out.

use std::collections::{BTreeMap, BTreeSet};

use super::attach::{self, Files};
use super::inputs::Checked;
use super::seeds::Seeded;
use crate::spec::env::Caller;
use crate::spec::graph::{RegionDef, RegionKind, RegionLayoutDef, RunGraph};
use crate::spec::inputs::{InputSlot, InputValue};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::RegionName;
use crate::spec::request::SpawnRequest;
use crate::spec::run_spec::SeededContent;
use crate::state::context::PartState;

/// What inputs and attachments add to one region.
#[derive(Debug, Clone, Default)]
pub(super) struct Contribution {
    texts: Vec<String>,
    parts: Vec<PartState>,
}

impl Contribution {
    fn text(&mut self, text: String) {
        if !text.trim().is_empty() {
            self.texts.push(text);
        }
    }

    fn file(&mut self, part: PartState, caption: Option<&String>) {
        self.parts.push(part);
        self.text(caption.cloned().unwrap_or_default());
    }
}

/// What inputs and attachments add, by region.
pub(super) type Placed = BTreeMap<RegionName, Contribution>;

/// Every region's content at spawn, and the regions the required-region
/// check leaves alone (see [`Seeded::excused`]).
#[derive(Debug, Clone, Default)]
pub(super) struct Contents {
    pub(super) regions: BTreeMap<RegionName, SeededContent>,
    excused: BTreeSet<RegionName>,
}

/// Every layout in the graph with its path: the graph's, then each stage's
/// own.
pub(super) fn layouts<'a>(
    graph: &'a RunGraph,
    at: &SpecPath,
) -> Vec<(&'a RegionLayoutDef, SpecPath)> {
    let mut all = vec![(&graph.layout, at.field("layout"))];
    for stage in &graph.stages {
        if let Some(layout) = &stage.layout {
            let path = at.field("stages").key(stage.name.as_str()).field("layout");
            all.push((layout, path));
        }
    }
    all
}

/// A region by name, from any layout.
pub(super) fn region_def<'a>(graph: &'a RunGraph, name: &RegionName) -> Option<&'a RegionDef> {
    std::iter::once(&graph.layout)
        .chain(graph.stages.iter().filter_map(|s| s.layout.as_ref()))
        .flat_map(|l| &l.regions)
        .find(|r| r.name == *name)
}

/// The region an attachment goes to when it names none: the graph's pinned
/// `task` region, else its first pinned region.
fn task_region(graph: &RunGraph) -> Option<&RegionName> {
    let regions = &graph.layout.regions;
    regions
        .iter()
        .find(|r| r.name.as_str() == "task" && r.kind == RegionKind::Pinned)
        .or_else(|| regions.iter().find(|r| r.kind == RegionKind::Pinned))
        .map(|r| &r.name)
}

fn all_region_names(graph: &RunGraph) -> BTreeSet<String> {
    std::iter::once(&graph.layout)
        .chain(graph.stages.iter().filter_map(|s| s.layout.as_ref()))
        .flat_map(|l| &l.regions)
        .map(|r| r.name.to_string())
        .collect()
}

/// Place each input bound to a region, and each attachment no `file` input
/// takes.
pub(super) fn place(
    graph: &RunGraph,
    checked: &Checked,
    files: &Files,
    request: &SpawnRequest,
    issues: &mut SpawnIssues,
) -> Placed {
    let mut placed = Placed::new();
    let caption = |name: &str| {
        request
            .attachments
            .iter()
            .find(|a| a.name == name)
            .and_then(|a| a.caption.as_ref())
    };
    let mut taken = BTreeSet::new();
    for decl in &graph.inputs {
        let Some(value) = checked.values.get(decl.name.as_str()) else {
            continue;
        };
        let file = match value {
            InputValue::File(name) => {
                taken.insert(name.as_str());
                files.get(name)
            }
            _ => None,
        };
        for slot in &decl.binds {
            let InputSlot::Region(binding) = slot else {
                continue;
            };
            let text = match &binding.template {
                Some(template) => template.render(&checked.values),
                None => value.render_text(),
            };
            let entry = placed.entry(binding.region.clone()).or_default();
            entry.text(text);
            let Some(file) = file else { continue };
            let fits = region_def(graph, &binding.region)
                .is_none_or(|r| attach::accepts(&file.mime, &r.accepts));
            match fits {
                true => entry.file(file.part.clone(), caption(&file.name)),
                false => issues.push(refused_type(
                    SpecPath::root().field("inputs").key(decl.name.as_str()),
                    &binding.region,
                    file,
                    graph,
                )),
            }
        }
    }
    for file in files.0.iter().filter(|f| !taken.contains(f.name.as_str())) {
        let attachment = &request.attachments[file.index];
        let path = SpecPath::root()
            .field("attachments")
            .index(file.index)
            .field("region");
        let target = attachment.region.as_ref().or_else(|| task_region(graph));
        let Some(name) = target else {
            issues.push(
                SpawnIssue::new(
                    path,
                    IssueCode::Missing,
                    "this file names no region, and the graph has no pinned region to put it in",
                )
                .known(all_region_names(graph)),
            );
            continue;
        };
        match region_def(graph, name) {
            None => issues.push(
                SpawnIssue::new(
                    path,
                    IssueCode::Dangling,
                    format!("no region is named \"{name}\""),
                )
                .known(all_region_names(graph)),
            ),
            Some(region) if !attach::accepts(&file.mime, &region.accepts) => {
                issues.push(refused_type(path, name, file, graph));
            }
            Some(_) => placed
                .entry(name.clone())
                .or_default()
                .file(file.part.clone(), attachment.caption.as_ref()),
        }
    }
    placed
}

fn refused_type(
    path: SpecPath,
    region: &RegionName,
    file: &attach::File,
    graph: &RunGraph,
) -> SpawnIssue {
    let accepts = region_def(graph, region)
        .map(|r| r.accepts.clone())
        .unwrap_or_default();
    SpawnIssue::new(
        path,
        IssueCode::WrongType,
        format!("region \"{region}\" does not take files of this type"),
    )
    .got(format!("\"{}\", a file of type {}", file.name, file.mime))
    .known(accepts)
}

/// Put each region's seed and placed content together, in the order the
/// module documents.
pub(super) fn combine(seeded: Seeded, mut placed: Placed) -> Contents {
    let mut regions = BTreeMap::new();
    let names: BTreeSet<RegionName> = seeded
        .regions
        .keys()
        .chain(placed.keys())
        .cloned()
        .collect();
    let mut from_seeds = seeded.regions;
    for name in names {
        let seed = from_seeds.remove(&name).unwrap_or_default();
        let added = placed.remove(&name).unwrap_or_default();
        let mut texts = Vec::new();
        if !seed.text.trim().is_empty() {
            texts.push(seed.text);
        }
        texts.extend(added.texts);
        let mut parts = seed.parts;
        parts.extend(added.parts);
        let content = SeededContent {
            text: texts.join("\n\n"),
            parts,
        };
        if !content.text.is_empty() || !content.parts.is_empty() {
            regions.insert(name, content);
        }
    }
    Contents {
        regions,
        excused: seeded.excused,
    }
}

/// Refuse a spawn that leaves a required region empty when the spawn was its
/// one chance to be filled (an input binds to it, or a seed fills it), or
/// that hands a run which takes a task nothing to do.
///
/// A fan-out worker is exempt from both: its work item arrives in its task,
/// and the regions a caller must fill were filled by the caller of its
/// parent. A region whose seed was checked but not run is not judged, since
/// what it would hold is not known.
pub(super) fn require_filled(
    graph: &RunGraph,
    contents: &Contents,
    checked: &Checked,
    request: &SpawnRequest,
    caller: &Caller,
    at: &SpecPath,
    issues: &mut SpawnIssues,
) {
    if matches!(caller, Caller::Worker { .. }) {
        return;
    }
    if empty_task(graph, checked, request, issues) {
        return;
    }
    let mut seen = BTreeSet::new();
    for (layout, path) in layouts(graph, at) {
        for (i, region) in layout.regions.iter().enumerate() {
            let filled = contents.regions.contains_key(&region.name)
                || contents.excused.contains(&region.name);
            // A required region nothing fills at spawn (no input, no seed) is
            // the run's to fill, and is judged when its stage is left.
            let at_spawn =
                region.seed.is_some() || !inputs_bound_to(graph, &region.name).is_empty();
            if !region.required || !at_spawn || filled || !seen.insert(region.name.clone()) {
                continue;
            }
            let message = region
                .required_message
                .clone()
                .unwrap_or_else(|| format!("required region \"{}\" was not filled", region.name));
            issues.push(
                SpawnIssue::new(path.field("regions").index(i), IssueCode::Missing, message)
                    .hint("supply an input bound to it, or attach a file to it")
                    .known(inputs_bound_to(graph, &region.name)),
            );
        }
    }
}

/// The inputs whose values land in `region`.
fn inputs_bound_to(graph: &RunGraph, region: &RegionName) -> Vec<String> {
    graph
        .inputs
        .iter()
        .filter(|d| {
            d.binds
                .iter()
                .any(|s| matches!(s, InputSlot::Region(b) if b.region == *region))
        })
        .map(|d| d.name.to_string())
        .collect()
}

/// A graph that takes a `task`, handed a blank one and nothing else in any
/// region, would run with nothing to do: refuse it. A caller that filled
/// another region or attached a file has said what the run is for.
fn empty_task(
    graph: &RunGraph,
    checked: &Checked,
    request: &SpawnRequest,
    issues: &mut SpawnIssues,
) -> bool {
    let region_bound = |d: &&crate::spec::inputs::InputDecl| {
        d.binds.iter().any(|s| matches!(s, InputSlot::Region(_)))
    };
    let takes_task = graph
        .inputs
        .iter()
        .filter(region_bound)
        .any(|d| d.name.as_str() == "task");
    let blank = graph.inputs.iter().filter(region_bound).all(|d| {
        checked
            .values
            .get(d.name.as_str())
            .is_none_or(|v| v.render_text().trim().is_empty())
    });
    let empty = checked.ok && takes_task && blank && request.attachments.is_empty();
    if empty {
        issues.push(
            SpawnIssue::new(
                SpecPath::root().field("inputs").key("task"),
                IssueCode::Missing,
                "refusing to start with an empty task",
            )
            .hint("say what the run is for, or hand it a region or a file"),
        );
    }
    empty
}
