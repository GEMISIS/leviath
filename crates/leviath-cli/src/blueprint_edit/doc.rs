//! The document and the typed views read from it.
//!
//! Every view is derived on demand from the `toml_edit` document and never
//! cached, so a mutator cannot leave a view stale. A view surfaces only the
//! keys the editor knows how to write; whatever else a table carries stays
//! in the document and comes back out of [`ManifestDoc::to_toml`] as it went
//! in.

use leviath_blueprint::BlueprintFile;
use toml_edit::{ArrayOfTables, DocumentMut, Item, TableLike, Value};

use super::EditError;
use super::tables::{
    get_bool, get_count, get_str, get_strings, index_named, list_tables, list_tables_mut,
    named_mut, sub,
};

/// An `agent.toml` held as a document: comments, key order and formatting
/// included.
#[derive(Debug, Clone)]
pub(crate) struct ManifestDoc {
    doc: DocumentMut,
}

/// The `[blueprint]` table, and the graph's entry, as the editor shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentView {
    /// `name`, or empty when the table has none.
    pub name: String,
    /// `version`, or empty.
    pub version: String,
    /// `description`, or empty.
    pub description: String,
    /// `[graph] entry`, when written.
    pub entry_stage: Option<String>,
    /// The `provider/model` every stage tries first, when they all agree;
    /// `None` when they differ (or no stage names one). Not a key of the
    /// file: The Lair shows it as the agent's "default model" and writes it
    /// back to every stage.
    pub default_model: Option<String>,
}

/// A stage's `mode`, as the editor shows and sets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StageModeView {
    /// `autonomous`, or no `mode` at all.
    Autonomous,
    /// `interactive`.
    Interactive,
    /// `{ interactive_points = [...] }`.
    InteractivePoints,
    /// `{ fan_out = {...} }`.
    FanOut,
    /// `output`.
    Output,
    /// A spelling the runtime would reject; shown as written, never rewritten
    /// unless the author picks another.
    Other(String),
}

impl StageModeView {
    /// The mode's name in the file.
    pub(crate) fn as_str(&self) -> &str {
        match self {
            StageModeView::Autonomous => "autonomous",
            StageModeView::Interactive => "interactive",
            StageModeView::InteractivePoints => "interactive_points",
            StageModeView::FanOut => "fan_out",
            StageModeView::Output => "output",
            StageModeView::Other(s) => s,
        }
    }

    /// From the mode's name in the file.
    pub(crate) fn parse(s: &str) -> Self {
        match s {
            "autonomous" => StageModeView::Autonomous,
            "interactive" => StageModeView::Interactive,
            "interactive_points" => StageModeView::InteractivePoints,
            "fan_out" => StageModeView::FanOut,
            "output" => StageModeView::Output,
            other => StageModeView::Other(other.to_string()),
        }
    }

    /// The modes the editor offers, in the order it offers them.
    pub const CHOICES: [StageModeView; 4] = [
        StageModeView::Autonomous,
        StageModeView::InteractivePoints,
        StageModeView::FanOut,
        StageModeView::Output,
    ];

    /// What the editor calls the mode.
    pub(crate) fn label(&self) -> &str {
        match self {
            StageModeView::Autonomous => "Works alone",
            StageModeView::Interactive => "Interactive",
            StageModeView::InteractivePoints => "Checks in with you",
            StageModeView::FanOut => "Fans out to workers",
            StageModeView::Output => "Hands back the result",
            StageModeView::Other(s) => s,
        }
    }
}

/// Where a fan-out stage gets its workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerKind {
    /// `worker = { blueprint = { name = "..." } }`: another blueprint,
    /// installed or (as `{ blueprint_file = "/path" }`) read from a
    /// directory.
    Agent,
    /// `worker = { stage = "..." }`: a stage of this blueprint.
    Stage,
    /// `worker = { query = "..." }`: matched against installed blueprints at
    /// run time.
    Query,
}

impl WorkerKind {
    /// The key under `worker` the kind is written as.
    pub(crate) fn key(self) -> &'static str {
        match self {
            WorkerKind::Agent => "blueprint",
            WorkerKind::Stage => "stage",
            WorkerKind::Query => "query",
        }
    }
}

/// A fan-out stage's settings, under `mode = { fan_out = {...} }`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct FanOutView {
    /// What the workers run, and its value: a stage, a blueprint's name (or
    /// `name@digest`, or a directory), or a query.
    pub worker: Option<(WorkerKind, String)>,
    /// `merge_stage`.
    pub merge_stage: Option<String>,
    /// `max_workers`.
    pub max_workers: Option<u64>,
    /// `max_items`.
    pub max_items: Option<u64>,
    /// `on_worker_failure`: `continue` or `fail_all`.
    pub on_worker_failure: Option<String>,
}

/// One `[[graph.stages]]` entry as the editor shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageView {
    /// `name`.
    pub name: String,
    /// `mode`.
    pub mode: StageModeView,
    /// `description`, or empty.
    pub description: String,
    /// `max_iterations`.
    pub max_iterations: Option<u64>,
    /// `max_revisits`.
    pub max_revisits: Option<u64>,
    /// `allow_complete`, when written.
    pub allow_complete: Option<bool>,
    /// The `model.models` chain, each as `provider/model`, or the bare model
    /// name when the stage leaves the provider open.
    pub models: Vec<String>,
    /// `tools`: tool names and `@group`s.
    pub tools: Vec<String>,
    /// `connectors`: MCP servers whose whole tool set the stage may use.
    pub connectors: Vec<String>,
    /// `system_prompt`, or empty.
    pub system_prompt: String,
    /// `transition_prompt`, or empty.
    pub transition_prompt: String,
    /// The fan-out settings, when the mode is a fan-out.
    pub fan_out: FanOutView,
    /// Whether the stage declares a `layout` of its own instead of using the
    /// graph's.
    pub has_own_layout: bool,
    /// Whether no edge leaves the stage: the run can end here.
    pub is_terminal: bool,
    /// `input_accepts`: what the stage takes as parts when its regions do
    /// not already say.
    pub input_accepts: Vec<String>,
    /// `input_as_text`: types whose parts reach the model as text whatever
    /// it takes.
    pub input_as_text: Vec<String>,
    /// The files the stage declares it hands back, in declaration order.
    pub artifacts: Vec<super::mime::ArtifactView>,
    /// `output.format`, or empty.
    pub output_format: String,
    /// `tool_accepts`: each tool and what it may be handed, in document
    /// order.
    pub tool_accepts: Vec<(String, Vec<String>)>,
    /// `output_routing`: the mime patterns the model's produced parts are
    /// routed by, each to a region, in document order.
    pub output_routing: Vec<(String, String)>,
    /// `reset`: the regions emptied when the stage is entered, in the order
    /// written.
    pub context_reset: Vec<String>,
}

/// When a path is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeKind {
    /// A `hint` the model routes on, with no `when` (or `when = "always"`).
    Hint,
    /// No `when` and no `hint`: the run always continues here.
    Always,
    /// `when = "llm_choice"`.
    LlmChoice,
    /// `when = "error"`.
    Error,
    /// `when = "max_iterations"`.
    MaxIterations,
    /// `when = "stuck"`.
    Stuck,
    /// `when = "dead_end"`.
    DeadEnd,
}

impl EdgeKind {
    /// The kinds the editor offers, in the order it offers them.
    pub const CHOICES: [EdgeKind; 7] = [
        EdgeKind::Always,
        EdgeKind::Hint,
        EdgeKind::LlmChoice,
        EdgeKind::Error,
        EdgeKind::Stuck,
        EdgeKind::MaxIterations,
        EdgeKind::DeadEnd,
    ];

    /// The `when` spelling; `Hint` has none (it is a `hint` key).
    pub(crate) fn condition(self) -> Option<&'static str> {
        match self {
            EdgeKind::Hint => None,
            EdgeKind::Always => Some("always"),
            EdgeKind::LlmChoice => Some("llm_choice"),
            EdgeKind::Error => Some("error"),
            EdgeKind::MaxIterations => Some("max_iterations"),
            EdgeKind::Stuck => Some("stuck"),
            EdgeKind::DeadEnd => Some("dead_end"),
        }
    }

    /// The `when` spelling read back; `None` for one the editor does not
    /// know, which it then leaves alone.
    pub(crate) fn from_condition(s: &str) -> Option<Self> {
        Self::CHOICES.into_iter().find(|k| k.condition() == Some(s))
    }

    /// What the editor calls the kind.
    pub(crate) fn label(self) -> &'static str {
        match self {
            EdgeKind::Hint => "model decides (hint)",
            EdgeKind::Always => "always continue",
            EdgeKind::LlmChoice => "model decides",
            EdgeKind::Error => "on error",
            EdgeKind::MaxIterations => "after too many tries",
            EdgeKind::Stuck => "when stuck",
            EdgeKind::DeadEnd => "when nothing else is left",
        }
    }

    /// The short word the canvas puts on the edge.
    pub(crate) fn short(self) -> &'static str {
        match self {
            EdgeKind::Hint => "hint",
            EdgeKind::Always => "always",
            EdgeKind::LlmChoice => "model's choice",
            EdgeKind::Error => "on error",
            EdgeKind::MaxIterations => "too many tries",
            EdgeKind::Stuck => "when stuck",
            EdgeKind::DeadEnd => "dead end",
        }
    }
}

/// How context crosses a path: its `carry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TransformKind {
    /// Carry everything: `carry` absent or `"direct"`.
    Direct,
    /// Empty every clearable region: `"clear"`.
    Clear,
    /// Summarize the conversation: `{ compact = {} }`.
    Compact,
    /// Per-region rules: `{ custom = {...} }`.
    Custom,
    /// A spelling the editor does not know; shown as written.
    Other(String),
}

impl TransformKind {
    /// The choices the editor offers.
    pub const CHOICES: [TransformKind; 4] = [
        TransformKind::Direct,
        TransformKind::Clear,
        TransformKind::Compact,
        TransformKind::Custom,
    ];

    /// The name of the `carry` variant.
    pub(crate) fn as_str(&self) -> &str {
        match self {
            TransformKind::Direct => "direct",
            TransformKind::Clear => "clear",
            TransformKind::Compact => "compact",
            TransformKind::Custom => "custom",
            TransformKind::Other(s) => s,
        }
    }

    /// From the name of the `carry` variant (absent reads as `Direct`).
    pub(crate) fn parse(s: &str) -> Self {
        match s {
            "" | "direct" => TransformKind::Direct,
            "clear" => TransformKind::Clear,
            "compact" => TransformKind::Compact,
            "custom" => TransformKind::Custom,
            other => TransformKind::Other(other.to_string()),
        }
    }

    /// What the editor calls it.
    pub(crate) fn label(&self) -> &str {
        match self {
            TransformKind::Direct => "Carry everything",
            TransformKind::Clear => "Keep only pinned",
            TransformKind::Compact => "Summarize everything",
            TransformKind::Custom => "Per-region rules",
            TransformKind::Other(s) => s,
        }
    }
}

/// A path's per-region rules, `carry = { custom = {...} }`, as typed lists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TransformRules {
    /// Regions carried as they are.
    pub carry: Vec<String>,
    /// Regions summarized.
    pub compact: Vec<String>,
    /// Regions emptied.
    pub clear: Vec<String>,
    /// The summarizing instructions (`compact_prompt` of a custom carry,
    /// `prompt` of a compacting one), or empty.
    pub compact_prompt: String,
    /// Whether the rules table exists at all: the cue to seed it when a path
    /// first turns custom.
    pub present: bool,
}

/// One `[[graph.edges]]` entry as the editor shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeView {
    /// The stage the path leaves.
    pub from: String,
    /// The stage it enters.
    pub to: String,
    /// When it is taken.
    pub kind: EdgeKind,
    /// The `hint`, when written (kept even under a `when`).
    pub hint: Option<String>,
    /// Whether a `gate` is written.
    pub gated: bool,
    /// How context crosses.
    pub transform: TransformKind,
    /// The per-region rules.
    pub rules: TransformRules,
}

/// One region of a layout as the editor shows it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RegionView {
    /// `name`.
    pub name: String,
    /// The kind's name (`pinned`, `sliding_window`, ...), or empty.
    pub kind: String,
    /// The percentage of a `budget = "N%"` (or `{ percent = "N%" }`), when
    /// it parses.
    pub budget_percent: Option<f64>,
    /// The budget's `max`, the absolute ceiling on a percentage.
    ///
    /// Usually absent: the percentage is what decides a region's size, and a
    /// ceiling below it just clamps the region on every model large enough to
    /// matter. Set it only where a region genuinely must not grow.
    pub max_tokens: Option<u64>,
    /// The budget's `min`, the absolute floor under a percentage.
    ///
    /// The counterpart to `max_tokens`, and the one small pinned regions want:
    /// a research question needs its ~1000 tokens whatever the model's window
    /// is, and a percentage of a narrow window would not give it them.
    pub min_tokens: Option<u64>,
    /// `required = true`.
    pub required: bool,
    /// `required_message`, or empty.
    pub required_message: String,
    /// The input that fills the region at spawn: the first `[[graph.inputs]]`
    /// that binds to it, or empty.
    pub seed: String,
    /// The region has a `seed` of its own (files, a command, code) the editor
    /// cannot display, so the seed field is left alone.
    pub seed_is_table: bool,
    /// The kind's `max_items` (sliding windows).
    pub max_items: Option<u64>,
    /// How a sliding window evicts: `per_item`, `bulk` or `compact`, or
    /// empty when the kind does not say.
    pub strategy: String,
    /// The count a `bulk` or `compact` eviction carries.
    pub overflow: Option<u64>,
    /// `description`, or empty.
    pub description: String,
    /// `accepts`: the mime type patterns the region takes; empty is
    /// anything.
    pub accepts: Vec<String>,
}

/// The layout a stage runs with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EffectiveRegions {
    /// The regions, in document order.
    pub regions: Vec<RegionView>,
    /// `true` when they are the graph's shared regions, `false` when the
    /// stage has a `layout` of its own.
    pub inherited: bool,
}

/// A stage's `tool_routing`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ToolRouting {
    /// `default_region`.
    pub default_region: Option<String>,
    /// `tool_regions`, tool to region, in document order.
    pub overrides: Vec<(String, String)>,
}

impl ManifestDoc {
    /// Read an `agent.toml`. Refuses text that is not TOML, has no
    /// `[blueprint]` table, or has no stage: the editor needs those to stand
    /// on.
    pub(crate) fn parse(text: &str) -> Result<Self, EditError> {
        let doc: DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| EditError::Toml(e.to_string()))?;
        if doc.get("blueprint").and_then(Item::as_table_like).is_none() {
            return Err(EditError::NoBlueprint);
        }
        let has_stage = doc
            .get("graph")
            .and_then(Item::as_table_like)
            .and_then(|g| g.get("stages"))
            .is_some_and(|s| !list_tables(s).is_empty());
        if !has_stage {
            return Err(EditError::NoStages);
        }
        Ok(Self { doc })
    }

    /// The file's text, exactly as it will be written.
    pub(crate) fn to_toml(&self) -> String {
        self.doc.to_string()
    }

    /// The file as the runtime reads it, or the reader's error.
    pub(crate) fn file(&self) -> Result<BlueprintFile, String> {
        BlueprintFile::parse(&self.to_toml())
    }

    pub(super) fn doc_mut(&mut self) -> &mut DocumentMut {
        &mut self.doc
    }

    /// The `[blueprint]` table.
    pub(super) fn meta(&self) -> &dyn TableLike {
        self.doc
            .get("blueprint")
            .and_then(Item::as_table_like)
            .expect("parse() checked [blueprint] is a table")
    }

    /// The `[blueprint]` table, mutably.
    pub(super) fn meta_mut(&mut self) -> &mut dyn TableLike {
        self.doc
            .get_mut("blueprint")
            .and_then(Item::as_table_like_mut)
            .expect("parse() checked [blueprint] is a table")
    }

    /// The `[graph]` table.
    pub(super) fn graph(&self) -> &dyn TableLike {
        self.doc
            .get("graph")
            .and_then(Item::as_table_like)
            .expect("parse() checked [graph] is a table")
    }

    /// The `[graph]` table, mutably.
    pub(super) fn graph_mut(&mut self) -> &mut dyn TableLike {
        self.doc
            .get_mut("graph")
            .and_then(Item::as_table_like_mut)
            .expect("parse() checked [graph] is a table")
    }

    /// `graph.stages`.
    pub(super) fn stages_list(&self) -> &Item {
        self.graph()
            .get("stages")
            .expect("parse() checked there are stages")
    }

    /// `graph.stages`, mutably.
    pub(super) fn stages_list_mut(&mut self) -> &mut Item {
        self.graph_mut()
            .get_mut("stages")
            .expect("parse() checked there are stages")
    }

    /// Whether the stages are `[[graph.stages]]` tables (and so new lists
    /// beside them are written that way too).
    pub(super) fn headed(&self) -> bool {
        self.stages_list().is_array_of_tables()
    }

    /// Where the stage called `name` sits in `graph.stages`.
    pub(super) fn stage_index(&self, name: &str) -> Option<usize> {
        index_named(self.stages_list(), name)
    }

    /// The stage called `name`.
    pub(super) fn stage_table(&self, name: &str) -> Option<&dyn TableLike> {
        list_tables(self.stages_list())
            .into_iter()
            .find(|t| get_str(*t, "name") == Some(name))
    }

    /// The stage called `name`, mutably, or [`EditError::NoSuchStage`].
    pub(super) fn stage_table_mut(&mut self, name: &str) -> Result<&mut dyn TableLike, EditError> {
        named_mut(self.stages_list_mut(), name)
            .ok_or_else(|| EditError::NoSuchStage(name.to_string()))
    }

    /// A list under `[graph]` (`edges`, `inputs`), when there is one.
    pub(super) fn graph_list(&self, key: &str) -> Option<&Item> {
        self.graph().get(key)
    }

    /// A list under `[graph]`, mutably, created in the stages' shape when
    /// missing. Refuses a key that holds something other than a list.
    pub(super) fn graph_list_mut(&mut self, key: &str) -> Result<&mut Item, EditError> {
        let headed = self.headed();
        let graph = self.graph_mut();
        if !graph.contains_key(key) {
            let empty = match headed {
                true => Item::ArrayOfTables(ArrayOfTables::new()),
                false => Item::Value(Value::Array(toml_edit::Array::new())),
            };
            graph.insert(key, empty);
        }
        let list = graph.get_mut(key).expect("present or inserted just above");
        match list.is_array_of_tables() || list.is_array() {
            true => Ok(list),
            false => Err(EditError::NotATable(key.to_string())),
        }
    }

    /// The `[blueprint]` view.
    pub(crate) fn agent(&self) -> AgentView {
        let meta = self.meta();
        let firsts: Vec<Option<String>> = self
            .stages()
            .iter()
            .map(|s| s.models.first().cloned())
            .collect();
        let default_model = match firsts.first() {
            Some(Some(first)) if firsts.iter().all(|m| m.as_ref() == Some(first)) => {
                Some(first.clone())
            }
            _ => None,
        };
        AgentView {
            name: get_str(meta, "name").unwrap_or_default().to_string(),
            version: get_str(meta, "version").unwrap_or_default().to_string(),
            description: get_str(meta, "description").unwrap_or_default().to_string(),
            entry_stage: get_str(self.graph(), "entry").map(str::to_string),
            default_model,
        }
    }

    /// The stage names, in the order the graph lists them.
    pub(crate) fn stage_names(&self) -> Vec<String> {
        list_tables(self.stages_list())
            .into_iter()
            .filter_map(|t| get_str(t, "name").map(str::to_string))
            .collect()
    }

    /// Whether a stage of that name exists.
    pub(crate) fn has_stage(&self, name: &str) -> bool {
        self.stage_table(name).is_some()
    }

    /// One stage's view.
    pub(crate) fn stage(&self, name: &str) -> Option<StageView> {
        let table = self.stage_table(name)?;
        let is_terminal = !self.edges().iter().any(|e| e.from == name);
        Some(stage_view(name, table, is_terminal))
    }

    /// Every stage's view, in graph order.
    pub(crate) fn stages(&self) -> Vec<StageView> {
        self.stage_names()
            .iter()
            .filter_map(|n| self.stage(n))
            .collect()
    }

    /// Every path, in file order. A path whose `when` the editor does not
    /// know is left out (and left alone).
    pub(crate) fn edges(&self) -> Vec<EdgeView> {
        self.graph_list("edges")
            .map(list_tables)
            .unwrap_or_default()
            .into_iter()
            .filter_map(edge_view)
            .collect()
    }

    /// The first path from `from` to `to`.
    pub(crate) fn edge(&self, from: &str, to: &str) -> Option<EdgeView> {
        self.edges()
            .into_iter()
            .find(|e| e.from == from && e.to == to)
    }

    /// The layout table of a scope: the graph's, or a stage's own.
    pub(super) fn layout_table(&self, stage: Option<&str>) -> Option<&dyn TableLike> {
        match stage {
            None => sub(self.graph(), "layout"),
            Some(name) => self.stage_table(name).and_then(|s| sub(s, "layout")),
        }
    }

    /// The regions of a scope, in document order; empty when the scope has
    /// no layout.
    pub(crate) fn regions(&self, stage: Option<&str>) -> Vec<RegionView> {
        let bindings = self.region_bindings();
        self.layout_table(stage)
            .and_then(|l| l.get("regions"))
            .map(list_tables)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|t| region_view(t, &bindings))
            .collect()
    }

    /// One region's view.
    pub(crate) fn region(&self, stage: Option<&str>, name: &str) -> Option<RegionView> {
        self.regions(stage).into_iter().find(|r| r.name == name)
    }

    /// The layout a stage runs with: its own when it declares one, the
    /// graph's otherwise. `None` asks for the graph's.
    pub(crate) fn effective_regions(&self, stage: Option<&str>) -> EffectiveRegions {
        match stage {
            Some(name) if self.layout_table(Some(name)).is_some() => EffectiveRegions {
                regions: self.regions(Some(name)),
                inherited: false,
            },
            _ => EffectiveRegions {
                regions: self.regions(None),
                inherited: true,
            },
        }
    }

    /// Each input's name with the regions it fills, in declaration order.
    pub(super) fn region_bindings(&self) -> Vec<(String, Vec<String>)> {
        self.graph_list("inputs")
            .map(list_tables)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|input| {
                let name = get_str(input, "name")?.to_string();
                Some((name, bound_regions(input)))
            })
            .collect()
    }

    /// A stage's tool routing; empty when it has none.
    pub(crate) fn tool_routing(&self, stage: &str) -> ToolRouting {
        let Some(routing) = self.stage_table(stage).and_then(|s| sub(s, "tool_routing")) else {
            return ToolRouting::default();
        };
        let overrides = sub(routing, "tool_regions")
            .map(|o| {
                o.iter()
                    .filter_map(|(tool, region)| {
                        region.as_str().map(|r| (tool.to_string(), r.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        ToolRouting {
            default_region: get_str(routing, "default_region").map(str::to_string),
            overrides,
        }
    }

    /// The stages whose tool routing (default or per tool) lands in
    /// `region`: what deleting the region would break.
    pub(crate) fn stages_routing_into(&self, region: &str) -> Vec<String> {
        self.stage_names()
            .into_iter()
            .filter(|s| {
                let r = self.tool_routing(s);
                r.default_region.as_deref() == Some(region)
                    || r.overrides.iter().any(|(_, to)| to == region)
            })
            .collect()
    }

    /// Every tool named by any stage, sorted and deduplicated.
    pub(crate) fn known_tools(&self) -> Vec<String> {
        let mut tools: Vec<String> = self.stages().into_iter().flat_map(|s| s.tools).collect();
        tools.sort();
        tools.dedup();
        tools
    }

    /// Every `provider/model` named by any stage, sorted and deduplicated.
    pub(crate) fn known_models(&self) -> Vec<String> {
        let mut models: Vec<String> = self.stages().into_iter().flat_map(|s| s.models).collect();
        models.sort();
        models.dedup();
        models
    }
}

/// The regions an input's `binds` fill: every `{ region = "..." }` entry.
pub(super) fn bound_regions(input: &dyn TableLike) -> Vec<String> {
    input
        .get("binds")
        .and_then(Item::as_array)
        .map(|binds| {
            binds
                .iter()
                .filter_map(Value::as_inline_table)
                .filter_map(|b| b.get("region").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn stage_view(name: &str, table: &dyn TableLike, is_terminal: bool) -> StageView {
    let output = sub(table, "output");
    StageView {
        name: name.to_string(),
        mode: mode_of(table),
        description: get_str(table, "description")
            .unwrap_or_default()
            .to_string(),
        max_iterations: get_count(table, "max_iterations"),
        max_revisits: get_count(table, "max_revisits"),
        allow_complete: get_bool(table, "allow_complete"),
        models: model_chain(table.get("model")),
        tools: get_strings(table, "tools"),
        connectors: get_strings(table, "connectors"),
        system_prompt: get_str(table, "system_prompt")
            .unwrap_or_default()
            .to_string(),
        transition_prompt: get_str(table, "transition_prompt")
            .unwrap_or_default()
            .to_string(),
        fan_out: fan_out_table(table).map(fan_out_view).unwrap_or_default(),
        has_own_layout: sub(table, "layout").is_some(),
        is_terminal,
        input_accepts: get_strings(table, "input_accepts"),
        input_as_text: get_strings(table, "input_as_text"),
        artifacts: super::mime::artifacts_of(table),
        output_format: output
            .and_then(|o| get_str(o, "format"))
            .unwrap_or_default()
            .to_string(),
        tool_accepts: super::mime::tool_limits_of(table),
        output_routing: sub(table, "output_routing")
            .map(|routing| {
                routing
                    .iter()
                    .filter_map(|(pattern, region)| {
                        region
                            .as_str()
                            .map(|r| (pattern.to_string(), r.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        context_reset: get_strings(table, "reset"),
    }
}

/// A stage's mode: a name, or a table whose one key is the mode
/// (`{ fan_out = {...} }`).
fn mode_of(stage: &dyn TableLike) -> StageModeView {
    let Some(mode) = stage.get("mode") else {
        return StageModeView::Autonomous;
    };
    if let Some(name) = mode.as_str() {
        return StageModeView::parse(name);
    }
    mode.as_table_like()
        .and_then(|t| t.iter().next().map(|(k, _)| StageModeView::parse(k)))
        .unwrap_or(StageModeView::Other(String::new()))
}

/// A stage's `mode.fan_out` table, when its mode is a fan-out.
pub(super) fn fan_out_table(stage: &dyn TableLike) -> Option<&dyn TableLike> {
    sub(stage, "mode").and_then(|m| sub(m, "fan_out"))
}

fn fan_out_view(fan_out: &dyn TableLike) -> FanOutView {
    FanOutView {
        worker: sub(fan_out, "worker").and_then(worker_of),
        merge_stage: get_str(fan_out, "merge_stage").map(str::to_string),
        max_workers: get_count(fan_out, "max_workers"),
        max_items: get_count(fan_out, "max_items"),
        on_worker_failure: get_str(fan_out, "on_worker_failure").map(str::to_string),
    }
}

/// What a `worker` table names. A blueprint reads as `name` or
/// `name@digest`, a blueprint directory as its path.
fn worker_of(worker: &dyn TableLike) -> Option<(WorkerKind, String)> {
    if let Some(stage) = get_str(worker, "stage") {
        return Some((WorkerKind::Stage, stage.to_string()));
    }
    if let Some(query) = get_str(worker, "query") {
        return Some((WorkerKind::Query, query.to_string()));
    }
    if let Some(path) = get_str(worker, "blueprint_file") {
        return Some((WorkerKind::Agent, path.to_string()));
    }
    let blueprint = sub(worker, "blueprint")?;
    let name = get_str(blueprint, "name")?;
    Some(match get_str(blueprint, "digest") {
        Some(digest) => (WorkerKind::Agent, format!("{name}@{digest}")),
        None => (WorkerKind::Agent, name.to_string()),
    })
}

/// The model chain a stage's `model` value stands for, each entry rendered as
/// `provider/model` when the blueprint pins a route and as a bare model name
/// when it leaves the route open.
fn model_chain(value: Option<&Item>) -> Vec<String> {
    let Some(models) = value
        .and_then(Item::as_table_like)
        .and_then(|t| t.get("models"))
        .and_then(Item::as_array)
    else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|entry| {
            if let Some(name) = entry.as_str() {
                return Some(name.to_string());
            }
            let t = entry.as_inline_table()?;
            let model = t.get("model").and_then(Value::as_str)?;
            // An empty provider is the same statement as omitting it.
            match t.get("provider").and_then(Value::as_str) {
                Some(provider) if !provider.is_empty() => Some(format!("{provider}/{model}")),
                _ => Some(model.to_string()),
            }
        })
        .collect()
}

fn edge_view(table: &dyn TableLike) -> Option<EdgeView> {
    let from = get_str(table, "from")?.to_string();
    let to = get_str(table, "to")?.to_string();
    let hint = get_str(table, "hint").map(str::to_string);
    let kind = match get_str(table, "when") {
        None | Some("always") if hint.is_some() => EdgeKind::Hint,
        None => EdgeKind::Always,
        Some(when) => EdgeKind::from_condition(when)?,
    };
    let carry = table.get("carry");
    let transform = match carry {
        None => TransformKind::Direct,
        Some(c) => match c.as_str() {
            Some(name) => TransformKind::parse(name),
            None => c
                .as_table_like()
                .and_then(|t| t.iter().next().map(|(k, _)| TransformKind::parse(k)))
                .unwrap_or(TransformKind::Other(String::new())),
        },
    };
    let carry = carry.and_then(Item::as_table_like);
    let custom = carry.and_then(|c| sub(c, "custom"));
    let compact_prompt = custom
        .and_then(|c| get_str(c, "compact_prompt"))
        .or_else(|| {
            carry
                .and_then(|c| sub(c, "compact"))
                .and_then(|c| get_str(c, "prompt"))
        })
        .unwrap_or_default()
        .to_string();
    let rules = TransformRules {
        carry: custom.map(|c| get_strings(c, "carry")).unwrap_or_default(),
        compact: custom
            .map(|c| get_strings(c, "compact"))
            .unwrap_or_default(),
        clear: custom.map(|c| get_strings(c, "clear")).unwrap_or_default(),
        compact_prompt,
        present: custom.is_some(),
    };
    Some(EdgeView {
        from,
        to,
        kind,
        hint,
        gated: table.contains_key("gate"),
        transform,
        rules,
    })
}

fn region_view(table: &dyn TableLike, bindings: &[(String, Vec<String>)]) -> Option<RegionView> {
    let name = get_str(table, "name")?.to_string();
    let kind = table.get("kind");
    let kind_table = kind.and_then(Item::as_table_like);
    let kind_name = kind
        .and_then(Item::as_str)
        .or_else(|| kind_table.and_then(|k| get_str(k, "kind")))
        .unwrap_or_default()
        .to_string();
    let eviction = kind_table.and_then(|k| k.get("eviction"));
    let (strategy, overflow) = match eviction {
        None => (String::new(), None),
        Some(e) => match e.as_str() {
            Some(name) => (name.to_string(), None),
            None => e
                .as_table_like()
                .and_then(|t| t.iter().next())
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        v.as_integer().and_then(|n| u64::try_from(n).ok()),
                    )
                })
                .unwrap_or_default(),
        },
    };
    let budget = table.get("budget");
    let budget_table = budget.and_then(Item::as_table_like);
    let percent_text = budget
        .and_then(Item::as_str)
        .or_else(|| budget_table.and_then(|b| get_str(b, "percent")));
    let seed = bindings
        .iter()
        .find(|(_, regions)| regions.contains(&name))
        .map(|(input, _)| input.clone())
        .unwrap_or_default();
    Some(RegionView {
        kind: kind_name,
        budget_percent: percent_text.and_then(parse_percent),
        max_tokens: budget_table.and_then(|b| get_count(b, "max")),
        min_tokens: budget_table.and_then(|b| get_count(b, "min")),
        required: get_bool(table, "required") == Some(true),
        required_message: get_str(table, "required_message")
            .unwrap_or_default()
            .to_string(),
        seed,
        seed_is_table: table.contains_key("seed"),
        max_items: kind_table.and_then(|k| get_count(k, "max_items")),
        strategy,
        overflow,
        description: get_str(table, "description")
            .unwrap_or_default()
            .to_string(),
        accepts: get_strings(table, "accepts"),
        name,
    })
}

/// `"35%"` as 35.0; anything else as `None`.
pub(super) fn parse_percent(s: &str) -> Option<f64> {
    s.trim()
        .strip_suffix('%')
        .and_then(|digits| digits.trim().parse().ok())
}

/// The edges list's tables, mutably; empty when there is none.
pub(super) fn edge_tables_mut(doc: &mut ManifestDoc) -> Vec<&mut dyn TableLike> {
    match doc.graph_mut().get_mut("edges") {
        Some(list) => list_tables_mut(list),
        None => Vec::new(),
    }
}
