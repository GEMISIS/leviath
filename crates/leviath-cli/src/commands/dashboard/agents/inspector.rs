//! What the inspector shows for the thing selected on the canvas, as a list
//! of fields; the same panels, field for field, as The Lair's properties
//! panel (This agent / Stage / Path), with the essentials it lacks (fan-out
//! settings, revisits, allow-complete) in the same rows.
//!
//! A field that does not apply is disabled rather than hidden, so the panel
//! never reflows under the cursor: the fan-out rows are there on every
//! stage and greyed until the mode is `fan_out`; the hint row is there on
//! every path and greyed unless the path is a hint.
//!
//! A region, a declared file and a stage's loop back to itself are edited
//! in a window over the editor rather than in the inspector's place: their
//! panels are the same lists of fields, drawn by the same code, opened over
//! the panel they came from and closed with Esc.

use crate::blueprint_edit::{
    EdgeKind, ManifestDoc, RegionScope, Rule, StageModeView, TransformKind, WorkerKind,
};

/// Which tab of the stage panel is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::commands::dashboard) enum StageTab {
    /// Mode, description, tries, prompts, fan-out.
    Behaviour,
    /// The regions the stage reads and what each takes, what it takes beyond
    /// them, what it reads as text, the answer's format and the files it
    /// hands back.
    Io,
    /// The model chain, the tools, and what each tool may be handed.
    Model,
    /// Regions and tool routing.
    Context,
}

impl StageTab {
    /// The four tabs, in order.
    pub(in crate::commands::dashboard) const ALL: [StageTab; 4] = [
        StageTab::Behaviour,
        StageTab::Io,
        StageTab::Model,
        StageTab::Context,
    ];

    /// The tab's title.
    pub(in crate::commands::dashboard) fn title(self) -> &'static str {
        match self {
            StageTab::Behaviour => "Behaviour",
            StageTab::Io => "Inputs & outputs",
            StageTab::Model => "Models & tools",
            StageTab::Context => "Context",
        }
    }

    /// The title when the four do not fit on one line of the inspector.
    pub(in crate::commands::dashboard) fn short_title(self) -> &'static str {
        match self {
            StageTab::Behaviour => "Behaviour",
            StageTab::Io => "In & out",
            StageTab::Model => "Models",
            StageTab::Context => "Context",
        }
    }

    /// The tab after `self`, wrapping round; `delta` of `-1` the one before.
    pub(in crate::commands::dashboard) fn step(self, delta: isize) -> StageTab {
        let at = StageTab::ALL.iter().position(|t| *t == self).unwrap_or(0) as isize;
        let n = StageTab::ALL.len() as isize;
        StageTab::ALL[(at + delta).rem_euclid(n) as usize]
    }
}

/// What the inspector is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands::dashboard) enum Panel {
    /// Nothing selected: the agent itself.
    Agent,
    /// A stage box.
    Stage { name: String, tab: StageTab },
    /// A path between two stages.
    Edge { from: String, to: String },
    /// A worker blueprint drawn on the canvas, which is edited elsewhere.
    External(String),
    /// A context region of a layout, opened in a window from a region row.
    Region { scope: RegionScope, name: String },
    /// One of a stage's declared files, opened in a window from its row.
    Artifact { stage: String, index: usize },
}

/// One thing the inspector can edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands::dashboard) enum FieldId {
    AgentDescription,
    EntryStage,
    DefaultModel,
    /// A shared region, by name (opens it; a later change).
    RegionRow(String),
    StageName,
    StageMode,
    StageDescription,
    MaxIterations,
    MaxRevisits,
    /// The stage's path back to itself (a row on its behaviour tab).
    SelfLoop,
    AllowComplete,
    WorkerKind,
    WorkerRef,
    MergeStage,
    MaxWorkers,
    MaxItems,
    OnWorkerFailure,
    MoveUp,
    MoveDown,
    DeleteStage,
    EdgeKind,
    EdgeHint,
    EdgeGate,
    /// How context crosses the path.
    EdgeTransform,
    /// One region's rule under a custom transform.
    TransformRule(String),
    CompactPrompt,
    DeletePath,
    /// The stage's own prompts, in the overlay.
    EditPrompts,
    /// One entry of the model chain (its index).
    ModelEntry(usize),
    AddModel,
    /// The tools the stage may use.
    ToolSet,
    /// Inherits the shared layout, or has its own: a status row.
    ContextStatus,
    /// A region of the stage's effective layout, by name.
    StageRegionRow(String),
    AddRegion,
    /// Give the stage its own layout, or drop it.
    OwnLayout,
    /// `tool_routing.default_region`.
    RoutingDefault,
    /// One `tool_routing.overrides` entry, by tool.
    RoutingRow(String),
    AddRouting,
    RegionName,
    RegionKind,
    RegionBudget,
    RegionMaxTokens,
    RegionMinTokens,
    RegionMaxItems,
    RegionStrategy,
    RegionOverflow,
    RegionRequired,
    RegionMessage,
    RegionSeed,
    RegionDescription,
    /// The media type patterns the region takes (a chooser).
    RegionAccepts,
    /// The most stored parts the region keeps.
    RegionMaxStored,
    DeleteRegion,
    /// A region the stage reads, on its inputs tab; opens the region.
    IoRegionRow(String),
    /// `[stages.<name>.input] accepts` (a chooser).
    StageAccepts,
    /// `[stages.<name>.input] as_text` (a chooser).
    StageAsText,
    /// `[stages.<name>.output] format`.
    OutputFormat,
    /// The stage's `n`th declared file; opens it, `x` drops it.
    ArtifactRow(usize),
    /// Declare another file (asks its name).
    AddArtifact,
    /// The open file's name.
    ArtifactName,
    /// The open file's type or pattern (a chooser).
    ArtifactType,
    /// Whether the open file must be present.
    ArtifactRequired,
    /// What the open file is for.
    ArtifactDescription,
    /// Drop the open file's declaration.
    DeleteArtifact,
    /// What one of the stage's tools may be handed (a chooser); `x` lifts
    /// the limit.
    ToolLimitRow(String),
}

/// What a field holds and how it edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands::dashboard) enum FieldValue {
    /// Free text: Enter opens a line editor.
    Text(String),
    /// A number: Enter opens a line editor, `←`/`→` step it.
    Number(Option<u64>),
    /// On or off: Enter, `←` or `→` flips it.
    Toggle(bool),
    /// One of a list: Enter opens the chooser, `←`/`→` cycle.
    Choice(String),
    /// A row that opens something (a region), or is removed with `x`.
    Row(String),
    /// Enter does the thing.
    Button,
    /// One of a few words, cycled in place with `←`/`→` or Enter.
    Segment {
        options: Vec<&'static str>,
        index: Option<usize>,
    },
}

/// One row of the inspector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands::dashboard) struct Field {
    pub(in crate::commands::dashboard) id: FieldId,
    pub(in crate::commands::dashboard) label: String,
    pub(in crate::commands::dashboard) value: FieldValue,
    /// One line under the panel while the cursor is on the row.
    pub(in crate::commands::dashboard) help: &'static str,
    /// Greyed and inert when the field does not apply.
    pub(in crate::commands::dashboard) enabled: bool,
}

impl Field {
    fn new(id: FieldId, label: impl Into<String>, value: FieldValue, help: &'static str) -> Self {
        Self {
            id,
            label: label.into(),
            value,
            help,
            enabled: true,
        }
    }

    fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
}

/// The inspector's rows for a panel, read from the document.
pub(in crate::commands::dashboard) fn fields(doc: &ManifestDoc, panel: &Panel) -> Vec<Field> {
    match panel {
        Panel::Agent => agent_fields(doc),
        Panel::Stage { name, tab } => stage_fields(doc, name, *tab),
        Panel::Edge { from, to } => edge_fields(doc, from, to),
        Panel::External(_) => Vec::new(),
        Panel::Region { scope, name } => region_fields(doc, scope, name),
        Panel::Artifact { stage, index } => artifact_fields(doc, stage, *index),
    }
}

/// What the panel is called.
pub(in crate::commands::dashboard) fn panel_title(panel: &Panel) -> String {
    match panel {
        Panel::Agent => "This agent".to_string(),
        Panel::Stage { name, tab } => format!("Stage · {name} · {}", tab.title()),
        Panel::Edge { from, to } if from == to => format!("Path · {from} ↺ back to itself"),
        Panel::Edge { from, to } => format!("Path · {from} → {to}"),
        Panel::External(name) => format!("Worker blueprint · {name}"),
        Panel::Region { scope, name } => match scope {
            RegionScope::Shared => format!("Context region · {name} · shared layout"),
            RegionScope::Stage(stage) => format!("Context region · {name} · {stage}'s own layout"),
        },
        Panel::Artifact { stage, index } => format!("File · {stage} hands back #{}", index + 1),
    }
}

/// A list of media types as a row reads it, or `empty` for none.
fn list_or(list: &[String], empty: &str) -> String {
    if list.is_empty() {
        empty.to_string()
    } else {
        list.join(", ")
    }
}

fn agent_fields(doc: &ManifestDoc) -> Vec<Field> {
    let agent = doc.agent();
    let mut out = vec![
        Field::new(
            FieldId::AgentDescription,
            "What this agent does",
            FieldValue::Text(agent.description),
            "One line, shown wherever the agent is listed.",
        ),
        Field::new(
            FieldId::EntryStage,
            "Starts at",
            FieldValue::Choice(
                agent
                    .entry_stage
                    .unwrap_or_else(|| "(first stage)".to_string()),
            ),
            "The stage a run begins in.",
        ),
        Field::new(
            FieldId::DefaultModel,
            "Default model",
            FieldValue::Choice(
                agent
                    .default_model
                    .unwrap_or_else(|| "Mixed: stages differ".to_string()),
            ),
            "The model every stage tries first. Picking one rewrites every stage's chain.",
        ),
    ];
    for region in doc.regions(None) {
        out.push(Field::new(
            FieldId::RegionRow(region.name.clone()),
            "Shared region",
            FieldValue::Row(format!(
                "{}  {}{}",
                region.name,
                region.kind,
                region_size(&region)
            )),
            "A context region every stage sees unless it has a layout of its own.",
        ));
    }
    out
}

fn stage_fields(doc: &ManifestDoc, name: &str, tab: StageTab) -> Vec<Field> {
    let Some(stage) = doc.stage(name) else {
        return Vec::new();
    };
    match tab {
        StageTab::Behaviour => {
            let fan_out = stage.mode == StageModeView::FanOut;
            let (worker_kind, worker_ref) = match &stage.fan_out.worker {
                Some((kind, value)) => (worker_kind_label(*kind).to_string(), value.clone()),
                None => ("(none)".to_string(), String::new()),
            };
            // A query is typed; a stage or an agent is picked from the ones
            // there are.
            let worker_value = match stage.fan_out.worker.map(|(k, _)| k) {
                Some(WorkerKind::Query) => FieldValue::Text(worker_ref),
                _ => FieldValue::Row(match worker_ref.is_empty() {
                    true => "(none)".to_string(),
                    false => worker_ref,
                }),
            };
            let names = doc.stage_names();
            let index = names.iter().position(|n| n == name).unwrap_or(0);
            let own_loop = doc.edge(name, name);
            let mut out = vec![
                Field::new(
                    FieldId::StageName,
                    "Name",
                    FieldValue::Text(stage.name.clone()),
                    "Renaming rewrites every path into the stage and the entry stage.",
                ),
                Field::new(
                    FieldId::StageMode,
                    "How it works",
                    FieldValue::Choice(stage.mode.label().to_string()),
                    "Works alone, checks in with you, fans out to workers, or hands back the result.",
                ),
                Field::new(
                    FieldId::StageDescription,
                    "What it does",
                    FieldValue::Text(stage.description),
                    "One line, shown on the stage's box and in listings.",
                ),
                Field::new(
                    FieldId::MaxIterations,
                    "Max tries",
                    FieldValue::Number(stage.max_iterations),
                    "Inference rounds before the stage is stopped. Empty leaves it to the runtime.",
                ),
                Field::new(
                    FieldId::MaxRevisits,
                    "Max revisits",
                    FieldValue::Number(stage.max_revisits),
                    "How often a run may come back to this stage. A stage that loops to itself needs one.",
                ),
                Field::new(
                    FieldId::AllowComplete,
                    "May finish the run",
                    FieldValue::Toggle(stage.allow_complete.unwrap_or(false)),
                    "The stage can call the run complete from inside, without a path out.",
                ),
            ];
            if let Some(edge) = own_loop {
                out.push(Field::new(
                    FieldId::SelfLoop,
                    "Loops back to itself",
                    FieldValue::Row(edge.kind.short().to_string()),
                    "A path from the stage to itself. Enter edits it like any other path.",
                ));
            }
            out.extend([
                Field::new(
                    FieldId::WorkerKind,
                    "Workers come from",
                    FieldValue::Choice(worker_kind),
                    "A stage of this agent, another installed agent, or a query matched at run time.",
                )
                .enabled(fan_out),
                Field::new(
                    FieldId::WorkerRef,
                    "Worker",
                    worker_value,
                    "The stage or the agent the workers run as (Enter picks one), or the \
                     query they are matched by (Enter types it). x clears it.",
                )
                .enabled(fan_out),
                Field::new(
                    FieldId::MergeStage,
                    "Merge stage",
                    FieldValue::Choice(
                        stage
                            .fan_out
                            .merge_stage
                            .clone()
                            .unwrap_or_else(|| "(none)".to_string()),
                    ),
                    "The stage that gathers the workers' results.",
                )
                .enabled(fan_out),
                Field::new(
                    FieldId::MaxWorkers,
                    "Max workers",
                    FieldValue::Number(stage.fan_out.max_workers),
                    "At once. Empty is the runtime's default; 0 is no limit.",
                )
                .enabled(fan_out),
                Field::new(
                    FieldId::MaxItems,
                    "Max items",
                    FieldValue::Number(stage.fan_out.max_items),
                    "How many pieces the work is split into at most.",
                )
                .enabled(fan_out),
                Field::new(
                    FieldId::OnWorkerFailure,
                    "If a worker fails",
                    FieldValue::Choice(
                        stage
                            .fan_out
                            .on_worker_failure
                            .clone()
                            .unwrap_or_else(|| "continue".to_string()),
                    ),
                    "Carry on with the rest, or fail the whole fan-out.",
                )
                .enabled(fan_out),
                Field::new(
                    FieldId::EditPrompts,
                    "Edit prompts",
                    FieldValue::Button,
                    "What the stage is told to do, and how it decides where to go next.",
                ),
                Field::new(
                    FieldId::MoveUp,
                    "Move up in the file",
                    FieldValue::Button,
                    "Order in the file only; the paths decide the flow.",
                )
                .enabled(index > 0),
                Field::new(
                    FieldId::MoveDown,
                    "Move down in the file",
                    FieldValue::Button,
                    "Order in the file only; the paths decide the flow.",
                )
                .enabled(index + 1 < names.len()),
                Field::new(
                    FieldId::DeleteStage,
                    "Delete stage",
                    FieldValue::Button,
                    "Removes the stage and every path in or out of it.",
                )
                .enabled(names.len() > 1),
            ]);
            out
        }
        StageTab::Model => {
            let mut out: Vec<Field> = stage
                .models
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    Field::new(
                        FieldId::ModelEntry(i),
                        if i == 0 { "Model" } else { "then" },
                        FieldValue::Row(m.clone()),
                        "Tried in order, best first. Enter swaps this one, x drops it, ←/→ or a drag on ⠿ moves it.",
                    )
                })
                .collect();
            if out.is_empty() {
                out.push(Field::new(
                    FieldId::AddModel,
                    "Model",
                    FieldValue::Row("(not set · runs on your default provider)".into()),
                    "Choose the model this stage runs on; Enter opens the list.",
                ));
            } else {
                out.push(Field::new(
                    FieldId::AddModel,
                    "Add a fallback model",
                    FieldValue::Button,
                    "Append a model to try when the ones above are not available.",
                ));
            }
            let tools = if stage.tools.is_empty() {
                "(none)".to_string()
            } else {
                stage.tools.join(", ")
            };
            out.push(Field::new(
                FieldId::ToolSet,
                "Tools it may use",
                FieldValue::Row(tools),
                "Enter picks from every tool this install has, or a group such as @builtin; \
                 Space toggles one.",
            ));
            // What each tool may be handed here: the tools named one by one
            // (a group has no row of its own), then any limit on a tool the
            // stage does not name, so it can still be lifted.
            let mut limited: Vec<String> = stage
                .tools
                .iter()
                .filter(|t| !t.starts_with('@'))
                .cloned()
                .collect();
            for (tool, _) in &stage.tool_accepts {
                if !limited.contains(tool) {
                    limited.push(tool.clone());
                }
            }
            for tool in limited {
                let limit = stage
                    .tool_accepts
                    .iter()
                    .find(|(t, _)| *t == tool)
                    .map(|(_, list)| list.join(", "));
                out.push(Field::new(
                    FieldId::ToolLimitRow(tool.clone()),
                    format!("  {tool} accepts"),
                    FieldValue::Row(limit.unwrap_or_else(|| "any type".to_string())),
                    "What this tool may be handed at this stage; a part of any other type is \
                     out of its reach here. Enter picks the types, x lifts the limit.",
                ));
            }
            out
        }
        StageTab::Context => {
            let layout = doc.effective_regions(Some(name));
            let status = if layout.inherited {
                "shared with the agent"
            } else {
                "its own layout"
            };
            let mut out = vec![Field::new(
                FieldId::ContextStatus,
                "Context",
                FieldValue::Row(status.to_string()),
                if layout.inherited {
                    "The stage sees the agent's shared regions, listed below."
                } else {
                    "The stage has regions of its own, listed below; the agent's shared ones are not seen here."
                },
            )];
            for region in &layout.regions {
                out.push(Field::new(
                    FieldId::StageRegionRow(region.name.clone()),
                    if layout.inherited { "  shared" } else { "  own" },
                    FieldValue::Row(format!("{}  {}{}", region.name, region.kind, region_size(region))),
                    if layout.inherited {
                        "Part of the shared layout; a change here is seen by every stage that inherits it."
                    } else {
                        "A region of this stage's own layout."
                    },
                ));
            }
            out.push(
                Field::new(
                    FieldId::AddRegion,
                    "Add a region",
                    FieldValue::Button,
                    "A new pinned region in this stage's own layout (asks its name).",
                )
                .enabled(!layout.inherited),
            );
            out.push(Field::new(
                FieldId::OwnLayout,
                if layout.inherited {
                    "Give this stage its own layout"
                } else {
                    "Back to the shared layout"
                },
                FieldValue::Button,
                if layout.inherited {
                    "A copy of the shared regions the stage can change without touching the others."
                } else {
                    "Back to inheriting the shared regions; the stage's own regions go."
                },
            ));
            let routing = doc.tool_routing(name);
            out.push(Field::new(
                FieldId::RoutingDefault,
                "Tool results land in",
                FieldValue::Choice(
                    routing
                        .default_region
                        .clone()
                        .unwrap_or_else(|| "(default)".to_string()),
                ),
                "Where a tool's output goes unless a row below says otherwise.",
            ));
            for (tool, region) in &routing.overrides {
                out.push(Field::new(
                    FieldId::RoutingRow(tool.clone()),
                    "  tool",
                    FieldValue::Row(format!("{tool} → {region}")),
                    "This tool's results land here. Enter changes the region, x stops routing it.",
                ));
            }
            out.push(Field::new(
                FieldId::AddRouting,
                "Route a tool",
                FieldValue::Button,
                "Send one tool's results to a region of their own.",
            ));
            out
        }
        StageTab::Io => io_fields(doc, name, &stage),
    }
}

/// The inputs and outputs tab, top to bottom: the input types (the
/// stage's own, or what its regions take), one row per region under it,
/// what is sent as text, the output type, and the output files.
fn io_fields(
    doc: &ManifestDoc,
    name: &str,
    stage: &crate::blueprint_edit::StageView,
) -> Vec<Field> {
    let layout = doc.effective_regions(Some(name));
    let mut out = Vec::new();
    // What the regions take between them, for the input row to show when
    // the stage says nothing of its own: any type when any region takes
    // anything, else the union of what they name (text aside, which every
    // stage takes).
    let mut from_regions: Vec<String> = Vec::new();
    let mut any = layout.regions.is_empty();
    for region in &layout.regions {
        if region.accepts.is_empty() {
            any = true;
        }
        for p in &region.accepts {
            if !p.starts_with("text/") && !from_regions.contains(p) {
                from_regions.push(p.clone());
            }
        }
    }
    let inherited = match any || from_regions.is_empty() {
        true => "any type".to_string(),
        false => from_regions.join(", "),
    };
    out.push(Field::new(
        FieldId::StageAccepts,
        "Input types",
        FieldValue::Row(match stage.input_accepts.is_empty() {
            true => format!("({inherited}, from its regions)"),
            false => stage.input_accepts.join(", "),
        }),
        "The files the stage takes, beyond text. Empty means whatever its regions take, \
         listed below. Enter picks types, x goes back to the regions.",
    ));
    for region in &layout.regions {
        out.push(Field::new(
            FieldId::IoRegionRow(region.name.clone()),
            format!("  from {}", region.name),
            FieldValue::Row(list_or(&region.accepts, "any type")),
            "A region the stage reads and the types it takes. Enter opens it in a window.",
        ));
    }
    out.push(Field::new(
        FieldId::StageAsText,
        "Sent as text",
        FieldValue::Row(list_or(&stage.input_as_text, "(none)")),
        "Files of these types reach the model as text even when it cannot take them \
         natively: model/obj, application/json. Enter picks types, x clears them.",
    ));
    out.push(Field::new(
        FieldId::OutputFormat,
        "Output type",
        FieldValue::Row(match stage.output_format.is_empty() {
            true => "(any)".to_string(),
            false => stage.output_format.clone(),
        }),
        "The shape of the answer: markdown, json, text, or a media type. Told to the model \
         and recorded with the result. Enter picks one, x asks for no shape.",
    ));
    for (i, artifact) in stage.artifacts.iter().enumerate() {
        out.push(Field::new(
            FieldId::ArtifactRow(i),
            "Output file",
            FieldValue::Row(format!(
                "{}  {}{}",
                artifact.name,
                artifact.media_type,
                if artifact.required { "  required" } else { "" }
            )),
            "A file the stage hands back beside its answer. Enter opens it in a window, x \
             drops the declaration.",
        ));
    }
    out.push(Field::new(
        FieldId::AddArtifact,
        "Declare an output file",
        FieldValue::Button,
        "A named file the stage must hand back (asks its name); the run checks it is there \
         and of the type.",
    ));
    out
}

/// The window for one declared file.
fn artifact_fields(doc: &ManifestDoc, stage: &str, index: usize) -> Vec<Field> {
    let Some(artifact) = doc.artifacts(stage).into_iter().nth(index) else {
        return Vec::new();
    };
    vec![
        Field::new(
            FieldId::ArtifactName,
            "Name",
            FieldValue::Text(artifact.name),
            "What the submission calls the file.",
        ),
        Field::new(
            FieldId::ArtifactType,
            "Type",
            FieldValue::Row(artifact.media_type),
            "The media type the file must be, or a pattern it must match: video/mp4, image/*. \
             Enter picks one.",
        ),
        Field::new(
            FieldId::ArtifactRequired,
            "Required",
            FieldValue::Toggle(artifact.required),
            "A submission without this file is refused back to the model.",
        ),
        Field::new(
            FieldId::ArtifactDescription,
            "Description",
            FieldValue::Text(artifact.description),
            "What the file is for, shown to the model.",
        ),
        Field::new(
            FieldId::DeleteArtifact,
            "Drop this declaration",
            FieldValue::Button,
            "The stage no longer promises this file.",
        ),
    ]
}

/// `· 5% · min 800 · max 4000`, whichever a region has.
///
/// Most regions now carry only the percentage - it is what decides the size,
/// and an absolute beside it is the exception rather than the shape.
fn region_size(region: &crate::blueprint_edit::RegionView) -> String {
    let mut s = String::new();
    if let Some(p) = region.budget_percent {
        s.push_str(&format!(" · {p}%"));
    }
    if let Some(t) = region.min_tokens {
        s.push_str(&format!(" · min {t}"));
    }
    if let Some(t) = region.max_tokens {
        s.push_str(&format!(" · max {t}"));
    }
    s
}

/// What the editor calls each region kind, and the line under it.
pub(in crate::commands::dashboard) const REGION_KINDS: [(&str, &str); 6] = [
    ("pinned", "Always kept, never evicted."),
    ("temporary", "Cleared when the stage hands off."),
    ("clearable", "The agent may wipe it when done with it."),
    ("sliding_window", "Keeps only the newest items."),
    ("compacting", "Summarizes itself when it fills up."),
    (
        "compact_history",
        "Stores the summaries squeezed out of a compacting region.",
    ),
];

fn region_fields(doc: &ManifestDoc, scope: &RegionScope, name: &str) -> Vec<Field> {
    let Some(region) = doc.region(scope.stage(), name) else {
        return Vec::new();
    };
    let sliding = region.kind == "sliding_window";
    let kind_help = REGION_KINDS
        .iter()
        .find(|(k, _)| *k == region.kind)
        .map(|(_, h)| *h)
        .unwrap_or("A kind this editor does not know; kept as written.");
    vec![
        Field::new(
            FieldId::RegionName,
            "Name",
            FieldValue::Text(region.name.clone()),
            "Renaming rewrites the tool routing that names it.",
        ),
        Field::new(
            FieldId::RegionKind,
            "How it behaves",
            FieldValue::Choice(region.kind.clone()),
            kind_help,
        ),
        Field::new(
            FieldId::RegionBudget,
            "Share of context (%)",
            FieldValue::Number(region.budget_percent.map(|p| p as u64)),
            "The share of the model's window this region may use. On its own it \
             scales with whatever model the stage runs.",
        ),
        Field::new(
            FieldId::RegionMaxTokens,
            "Token ceiling",
            FieldValue::Number(region.max_tokens),
            "Hard cap on the share above. Usually leave empty - a ceiling below \
             the percentage clamps the region on every large-window model.",
        ),
        Field::new(
            FieldId::RegionMinTokens,
            "Token floor",
            FieldValue::Number(region.min_tokens),
            "Floor under the share above, for a small region that needs its \
             tokens whatever the window is.",
        ),
        Field::new(
            FieldId::RegionMaxItems,
            "Window: keeps at most",
            FieldValue::Number(region.max_items),
            "How many items the window keeps.",
        )
        .enabled(sliding),
        Field::new(
            FieldId::RegionStrategy,
            "Window: strategy",
            FieldValue::Text(region.strategy.clone()),
            "per_item, bulk or compact.",
        )
        .enabled(sliding),
        Field::new(
            FieldId::RegionOverflow,
            "Window: overflow",
            FieldValue::Number(region.overflow),
            "How many items over the limit before it evicts.",
        )
        .enabled(sliding),
        Field::new(
            FieldId::RegionAccepts,
            "Takes",
            FieldValue::Row(list_or(&region.accepts, "any type")),
            "Media type patterns the region takes: image/*, audio/wav. A write outside them \
             is refused with the list. Enter picks the types, x takes anything again.",
        ),
        Field::new(
            FieldId::RegionMaxStored,
            "Stored parts: keeps at most",
            FieldValue::Number(region.max_stored),
            "How many stored parts the region holds across its entries; past it the \
             oldest goes. Empty is unbounded.",
        ),
        Field::new(
            FieldId::RegionRequired,
            "Must be filled first",
            FieldValue::Toggle(region.required),
            "The agent cannot move on while this region is empty.",
        ),
        Field::new(
            FieldId::RegionMessage,
            "Reminder if empty",
            FieldValue::Text(region.required_message.clone()),
            "What the run says when it is stopped by an empty required region.",
        )
        .enabled(region.required),
        Field::new(
            FieldId::RegionSeed,
            "Seeded with",
            FieldValue::Text(if region.seed_is_table {
                "(files, a command or a script: edit the file)".to_string()
            } else {
                region.seed.clone()
            }),
            "`task` means the user's request; another name, that caller input.",
        )
        .enabled(!region.seed_is_table),
        Field::new(
            FieldId::RegionDescription,
            "Description",
            FieldValue::Text(region.description.clone()),
            "What the region holds, for anyone reading the file.",
        ),
        Field::new(
            FieldId::DeleteRegion,
            "Delete region",
            FieldValue::Button,
            "Removes the region and stops routing tool results into it.",
        ),
    ]
}

fn edge_fields(doc: &ManifestDoc, from: &str, to: &str) -> Vec<Field> {
    let Some(edge) = doc.edge(from, to) else {
        return Vec::new();
    };
    vec![
        Field::new(
            FieldId::EdgeKind,
            "When to take it",
            FieldValue::Choice(edge.kind.label().to_string()),
            "Always, when the model decides, on error, when stuck, after too many tries, or as a last resort.",
        ),
        Field::new(
            FieldId::EdgeHint,
            "Hint for the model",
            FieldValue::Text(edge.hint.clone().unwrap_or_default()),
            "e.g. The work is done and verified",
        )
        .enabled(edge.kind == EdgeKind::Hint),
        Field::new(
            FieldId::EdgeGate,
            "Needs your approval",
            FieldValue::Toggle(edge.gated),
            "The run pauses on this path until you approve it.",
        ),
        Field::new(
            FieldId::EdgeTransform,
            "Context carried over",
            FieldValue::Choice(edge.transform.label().to_string()),
            "Pinned regions always survive an edge.",
        ),
    ]
    .into_iter()
    .chain(transform_rule_fields(doc, from, &edge))
    .chain([
        Field::new(
            FieldId::CompactPrompt,
            "How to summarize",
            FieldValue::Text(edge.rules.compact_prompt.clone()),
            "What the summary of the compacted regions should keep.",
        )
        .enabled(edge.transform == TransformKind::Custom),
        Field::new(
            FieldId::DeletePath,
            "Delete path",
            FieldValue::Button,
            "Removes the path; the stages stay.",
        ),
    ])
    .collect()
}

/// One segment row per region the leaving stage sees: carry / summarize /
/// drop, live only under a custom transform; a pinned region is always
/// carried and has no segment.
fn transform_rule_fields(
    doc: &ManifestDoc,
    from: &str,
    edge: &crate::blueprint_edit::EdgeView,
) -> Vec<Field> {
    let custom = edge.transform == TransformKind::Custom;
    doc.effective_regions(Some(from))
        .regions
        .into_iter()
        .map(|region| {
            if region.kind == "pinned" {
                return Field::new(
                    FieldId::TransformRule(region.name.clone()),
                    format!("  {}", region.name),
                    FieldValue::Row("always carried".to_string()),
                    "Pinned regions always survive an edge.",
                )
                .enabled(false);
            }
            let index = Rule::ALL.iter().position(|r| match r {
                Rule::Carry => edge.rules.carry.contains(&region.name),
                Rule::Compact => edge.rules.compact.contains(&region.name),
                Rule::Clear => edge.rules.clear.contains(&region.name),
            });
            Field::new(
                FieldId::TransformRule(region.name.clone()),
                format!("  {}", region.name),
                FieldValue::Segment {
                    options: vec!["Carry", "Summarize", "Drop"],
                    index,
                },
                "Carry it as it is, summarize it, or drop it on the way across.",
            )
            .enabled(custom)
        })
        .collect()
}

/// What the editor calls a worker source.
pub(in crate::commands::dashboard) fn worker_kind_label(kind: WorkerKind) -> &'static str {
    match kind {
        WorkerKind::Stage => "a stage of this agent",
        WorkerKind::Agent => "another agent",
        WorkerKind::Query => "a query at run time",
    }
}
