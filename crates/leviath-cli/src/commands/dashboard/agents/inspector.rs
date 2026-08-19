//! What the inspector shows for the thing selected on the canvas, as a list
//! of fields; the same panels, field for field, as The Lair's properties
//! panel (This agent / Stage / Path), with the essentials it lacks (fan-out
//! settings, revisits, allow-complete) in the same rows.
//!
//! A field that does not apply is disabled rather than hidden, so the panel
//! never reflows under the cursor: the fan-out rows are there on every
//! stage and greyed until the mode is `fan_out`; the hint row is there on
//! every path and greyed unless the path is a hint.

use crate::blueprint_edit::{EdgeKind, ManifestDoc, StageModeView, WorkerKind};

/// Which tab of the stage panel is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::commands::dashboard) enum StageTab {
    /// Mode, description, tries, prompts, fan-out.
    Behaviour,
    /// The model chain and the tools.
    Model,
    /// Regions and tool routing.
    Context,
}

impl StageTab {
    /// The three tabs, in order.
    pub(in crate::commands::dashboard) const ALL: [StageTab; 3] =
        [StageTab::Behaviour, StageTab::Model, StageTab::Context];

    /// The tab's title.
    pub(in crate::commands::dashboard) fn title(self) -> &'static str {
        match self {
            StageTab::Behaviour => "Behaviour",
            StageTab::Model => "Model & tools",
            StageTab::Context => "Context",
        }
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
    DeletePath,
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
    /// A row that opens something (a region).
    Row(String),
    /// Enter does the thing.
    Button,
}

/// One row of the inspector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands::dashboard) struct Field {
    pub(in crate::commands::dashboard) id: FieldId,
    pub(in crate::commands::dashboard) label: &'static str,
    pub(in crate::commands::dashboard) value: FieldValue,
    /// One line under the panel while the cursor is on the row.
    pub(in crate::commands::dashboard) help: &'static str,
    /// Greyed and inert when the field does not apply.
    pub(in crate::commands::dashboard) enabled: bool,
}

impl Field {
    fn new(id: FieldId, label: &'static str, value: FieldValue, help: &'static str) -> Self {
        Self {
            id,
            label,
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
    }
}

/// What the panel is called.
pub(in crate::commands::dashboard) fn panel_title(panel: &Panel) -> String {
    match panel {
        Panel::Agent => "This agent".to_string(),
        Panel::Stage { name, tab } => format!("Stage · {name} · {}", tab.title()),
        Panel::Edge { from, to } => format!("Path · {from} → {to}"),
        Panel::External(name) => format!("Worker blueprint · {name}"),
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
        let budget = region
            .budget_percent
            .map(|p| format!("{p}%"))
            .unwrap_or_default();
        let tokens = region
            .max_tokens
            .map(|t| format!("{t} tokens"))
            .unwrap_or_default();
        out.push(Field::new(
            FieldId::RegionRow(region.name.clone()),
            "Shared region",
            FieldValue::Row(format!(
                "{}  {}  {budget} {tokens}",
                region.name, region.kind
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
            let names = doc.stage_names();
            let index = names.iter().position(|n| n == name).unwrap_or(0);
            vec![
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
                    FieldValue::Text(worker_ref),
                    "The stage, agent or query the workers run as.",
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
            ]
        }
        StageTab::Model | StageTab::Context => Vec::new(),
    }
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
            "Hint the model routes on",
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
            FieldId::DeletePath,
            "Delete path",
            FieldValue::Button,
            "Removes the path; the stages stay.",
        ),
    ]
}

/// What the editor calls a worker source.
pub(in crate::commands::dashboard) fn worker_kind_label(kind: WorkerKind) -> &'static str {
    match kind {
        WorkerKind::Stage => "a stage of this agent",
        WorkerKind::Agent => "another agent",
        WorkerKind::Query => "a query at run time",
    }
}
