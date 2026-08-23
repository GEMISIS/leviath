//! The document and the typed views read from it.
//!
//! Every view is derived on demand from the `toml_edit` document and never
//! cached, so a mutator cannot leave a view stale. A view surfaces only the
//! keys the editor knows how to write; whatever else a table carries stays
//! in the document and comes back out of [`ManifestDoc::to_toml`] as it went
//! in.

use leviath_core::Blueprint;
use leviath_core::manifest::parse_manifest;
use toml_edit::{DocumentMut, Item, TableLike, Value};

use super::tables::{as_table, child, get_bool, get_int, get_str, get_strings, table_keys};
use super::{EditError, order};

/// An `agent.leviath` manifest held as a document: comments, key order and
/// formatting included.
#[derive(Debug, Clone)]
pub struct ManifestDoc {
    doc: DocumentMut,
}

/// The `[agent]` table as the editor shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentView {
    /// `name`, or empty when the table has none.
    pub name: String,
    /// `version`, or empty.
    pub version: String,
    /// `description`, or empty.
    pub description: String,
    /// `entry_stage`, when written.
    pub entry_stage: Option<String>,
    /// The `provider/model` every stage tries first, when they all agree;
    /// `None` when they differ (or no stage names one). Not a key of the
    /// manifest: The Lair shows it as the agent's "default model" and writes
    /// it back to every stage.
    pub default_model: Option<String>,
}

/// A stage's `mode`, as the editor shows and sets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageModeView {
    /// `autonomous`, or no `mode` at all.
    Autonomous,
    /// `interactive`.
    Interactive,
    /// `interactive_points`.
    InteractivePoints,
    /// `fan_out`.
    FanOut,
    /// `output`.
    Output,
    /// A spelling the runtime would reject; shown as written, never rewritten
    /// unless the author picks another.
    Other(String),
}

impl StageModeView {
    /// The manifest spelling.
    pub fn as_str(&self) -> &str {
        match self {
            StageModeView::Autonomous => "autonomous",
            StageModeView::Interactive => "interactive",
            StageModeView::InteractivePoints => "interactive_points",
            StageModeView::FanOut => "fan_out",
            StageModeView::Output => "output",
            StageModeView::Other(s) => s,
        }
    }

    /// From the manifest spelling.
    pub fn parse(s: &str) -> Self {
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
    pub fn label(&self) -> &str {
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
pub enum WorkerKind {
    /// `worker_agent`: a separate installed blueprint.
    Agent,
    /// `worker_stage`: a stage of this blueprint.
    Stage,
    /// `worker_query`: matched against installed blueprints at run time.
    Query,
}

impl WorkerKind {
    /// The manifest key.
    pub fn key(self) -> &'static str {
        match self {
            WorkerKind::Agent => "worker_agent",
            WorkerKind::Stage => "worker_stage",
            WorkerKind::Query => "worker_query",
        }
    }
}

/// A stage's fan-out keys. Meaningful when its mode is `fan_out`; read
/// regardless, so nothing is lost when a mode flips back and forth.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FanOutView {
    /// Which of `worker_agent`/`worker_stage`/`worker_query` is written, and
    /// its value; the first found when several are.
    pub worker: Option<(WorkerKind, String)>,
    /// `merge_stage`.
    pub merge_stage: Option<String>,
    /// `max_workers` (0 = unlimited).
    pub max_workers: Option<u64>,
    /// `max_items` (0 reads as unlimited too).
    pub max_items: Option<u64>,
    /// `on_worker_failure`: `continue` or `fail_all`.
    pub on_worker_failure: Option<String>,
}

/// One `[stages.<name>]` table as the editor shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageView {
    /// The table key.
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
    /// The `provider/model` fallback chain; a bare-string `model` reads as
    /// one entry.
    pub models: Vec<String>,
    /// `available_tools`.
    pub tools: Vec<String>,
    /// `system_prompt`, or empty.
    pub system_prompt: String,
    /// `transition_prompt`, or empty.
    pub transition_prompt: String,
    /// The fan-out keys.
    pub fan_out: FanOutView,
    /// Whether the stage declares `[stages.<name>.context.regions]` of its
    /// own instead of inheriting the agent's.
    pub has_own_layout: bool,
    /// Whether the stage has a `transitions` table with nothing in it: the
    /// run can end here.
    pub is_terminal: bool,
}

/// When a path is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// A `hint = "..."` the model routes on (no `condition` key).
    Hint,
    /// `condition = "always"`.
    Always,
    /// `condition = "llm_choice"`, or neither `condition` nor `hint`.
    LlmChoice,
    /// `condition = "error"`.
    Error,
    /// `condition = "max_iterations"`.
    MaxIterations,
    /// `condition = "stuck"`.
    Stuck,
    /// `condition = "dead_end"`.
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

    /// The `condition` spelling; `Hint` has none (it is a `hint` key).
    pub fn condition(self) -> Option<&'static str> {
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

    /// The `condition` spelling read back; `None` for one the editor does not
    /// know, which it then leaves alone.
    pub fn from_condition(s: &str) -> Option<Self> {
        Self::CHOICES.into_iter().find(|k| k.condition() == Some(s))
    }

    /// What the editor calls the kind.
    pub fn label(self) -> &'static str {
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
    pub fn short(self) -> &'static str {
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

/// How context crosses a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransformKind {
    /// Carry everything: `transform` absent or `"direct"`.
    Direct,
    /// Keep only pinned regions.
    Clear,
    /// Summarize everything: `"compact"`, and `"summarize"` until changed.
    Compact,
    /// Per-region rules in `transform_config`.
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

    /// The manifest spelling; `Direct` is written as absent.
    pub fn as_str(&self) -> &str {
        match self {
            TransformKind::Direct => "direct",
            TransformKind::Clear => "clear",
            TransformKind::Compact => "compact",
            TransformKind::Custom => "custom",
            TransformKind::Other(s) => s,
        }
    }

    /// From the manifest spelling (absent reads as `Direct`).
    pub fn parse(s: &str) -> Self {
        match s {
            "" | "direct" => TransformKind::Direct,
            "clear" => TransformKind::Clear,
            "compact" | "summarize" => TransformKind::Compact,
            "custom" => TransformKind::Custom,
            other => TransformKind::Other(other.to_string()),
        }
    }

    /// What the editor calls it.
    pub fn label(&self) -> &str {
        match self {
            TransformKind::Direct => "Carry everything",
            TransformKind::Clear => "Keep only pinned",
            TransformKind::Compact => "Summarize everything",
            TransformKind::Custom => "Per-region rules",
            TransformKind::Other(s) => s,
        }
    }
}

/// A path's `transform_config`, as typed lists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TransformRules {
    /// Regions carried as they are.
    pub carry: Vec<String>,
    /// Regions summarized.
    pub compact: Vec<String>,
    /// Regions dropped.
    pub clear: Vec<String>,
    /// `compact_prompt`, or empty.
    pub compact_prompt: String,
    /// Whether the table exists at all: the cue to seed it when a path first
    /// turns custom.
    pub present: bool,
}

/// One `[stages.<from>.transitions.<to>]` table as the editor shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeView {
    /// The stage the path leaves.
    pub from: String,
    /// The stage it enters.
    pub to: String,
    /// When it is taken.
    pub kind: EdgeKind,
    /// The `hint`, when written (kept even under a `condition`).
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
pub struct RegionView {
    /// The table key.
    pub name: String,
    /// `kind`, or empty.
    pub kind: String,
    /// The `budget = "N%"` percentage, when it parses.
    pub budget_percent: Option<f64>,
    /// `max_tokens`, the absolute ceiling on a percentage budget.
    ///
    /// Usually absent: the percentage is what decides a region's size, and a
    /// ceiling below it just clamps the region on every model large enough to
    /// matter. Set it only where a region genuinely must not grow.
    pub max_tokens: Option<u64>,
    /// `min_tokens`, the absolute floor under a percentage budget.
    ///
    /// The counterpart to `max_tokens`, and the one small pinned regions want:
    /// a research question needs its ~1000 tokens whatever the model's window
    /// is, and a percentage of a narrow window would not give it them.
    pub min_tokens: Option<u64>,
    /// `required = true`.
    pub required: bool,
    /// `required_message`, or empty.
    pub required_message: String,
    /// A string `seed`; a table-shaped seed reads as empty and is never
    /// clobbered (see `seed_is_table`).
    pub seed: String,
    /// The `seed` is a table (`{ files = [...] }`, ...) the editor cannot
    /// display, so clearing the field will not touch it.
    pub seed_is_table: bool,
    /// `max_items` (sliding windows).
    pub max_items: Option<u64>,
    /// `strategy` (sliding windows), or empty.
    pub strategy: String,
    /// `overflow` (sliding windows).
    pub overflow: Option<u64>,
    /// `description`, or empty.
    pub description: String,
}

/// The layout a stage runs with.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveRegions {
    /// The regions, in document order.
    pub regions: Vec<RegionView>,
    /// `true` when they are the agent's shared regions, `false` when the
    /// stage has its own `[stages.<name>.context.regions]`.
    pub inherited: bool,
}

/// A stage's `tool_routing`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolRouting {
    /// `default_region`.
    pub default_region: Option<String>,
    /// `overrides`, tool to region, in document order.
    pub overrides: Vec<(String, String)>,
}

impl ManifestDoc {
    /// Read a manifest. Refuses text that is not TOML, has no `[agent]`
    /// table, or has no stage table: the editor needs those to stand on.
    pub fn parse(text: &str) -> Result<Self, EditError> {
        let doc: DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| EditError::Toml(e.to_string()))?;
        if doc.get("agent").and_then(as_table).is_none() {
            return Err(EditError::NoAgent);
        }
        let this = Self { doc };
        if this.stage_names().is_empty() {
            return Err(EditError::NoStages);
        }
        Ok(this)
    }

    /// The manifest text, exactly as it will be written.
    pub fn to_toml(&self) -> String {
        self.doc.to_string()
    }

    /// The manifest as the runtime reads it, or the runtime's parse error.
    pub fn blueprint(&self) -> Result<Blueprint, String> {
        parse_manifest(&self.to_toml()).map_err(|e| e.to_string())
    }

    pub(super) fn doc(&self) -> &DocumentMut {
        &self.doc
    }

    pub(super) fn doc_mut(&mut self) -> &mut DocumentMut {
        &mut self.doc
    }

    /// The `[agent]` table.
    pub(super) fn agent_table(&self) -> &dyn TableLike {
        self.doc
            .get("agent")
            .and_then(as_table)
            .expect("parse() checked [agent] is a table")
    }

    /// The `[stages.<name>]` item.
    pub(super) fn stage_item(&self, name: &str) -> Option<&Item> {
        self.doc
            .get("stages")
            .and_then(Item::as_table_like)
            .and_then(|t| t.get(name))
            .filter(|i| i.as_table_like().is_some())
    }

    /// The `[stages.<name>]` item, mutably.
    pub(super) fn stage_item_mut(&mut self, name: &str) -> Option<&mut Item> {
        self.doc
            .get_mut("stages")
            .and_then(Item::as_table_like_mut)
            .and_then(|t| t.get_mut(name))
            .filter(|i| i.as_table_like().is_some())
    }

    /// The `[agent]` view.
    pub fn agent(&self) -> AgentView {
        let agent = self.agent_table();
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
            name: get_str(agent, "name").unwrap_or_default().to_string(),
            version: get_str(agent, "version").unwrap_or_default().to_string(),
            description: get_str(agent, "description")
                .unwrap_or_default()
                .to_string(),
            entry_stage: get_str(agent, "entry_stage").map(str::to_string),
            default_model,
        }
    }

    /// The stage names, in the order the file shows them.
    pub fn stage_names(&self) -> Vec<String> {
        order::stage_order(&self.doc)
    }

    /// Whether a stage of that name exists.
    pub fn has_stage(&self, name: &str) -> bool {
        self.stage_item(name).is_some()
    }

    /// One stage's view.
    pub fn stage(&self, name: &str) -> Option<StageView> {
        self.stage_item(name).map(|item| stage_view(name, item))
    }
}

fn stage_view(name: &str, item: &Item) -> StageView {
    {
        let table = as_table(item).expect("stage_item is a table");
        let transitions = child(item, "transitions");
        let worker = [WorkerKind::Agent, WorkerKind::Stage, WorkerKind::Query]
            .into_iter()
            .find_map(|k| get_str(table, k.key()).map(|v| (k, v.to_string())));
        StageView {
            name: name.to_string(),
            mode: get_str(table, "mode")
                .map(StageModeView::parse)
                .unwrap_or(StageModeView::Autonomous),
            description: get_str(table, "description")
                .unwrap_or_default()
                .to_string(),
            max_iterations: get_int(table, "max_iterations").and_then(|n| u64::try_from(n).ok()),
            max_revisits: get_int(table, "max_revisits").and_then(|n| u64::try_from(n).ok()),
            allow_complete: get_bool(table, "allow_complete"),
            models: model_chain(table.get("model")),
            tools: get_strings(table, "available_tools"),
            system_prompt: get_str(table, "system_prompt")
                .unwrap_or_default()
                .to_string(),
            transition_prompt: get_str(table, "transition_prompt")
                .unwrap_or_default()
                .to_string(),
            fan_out: FanOutView {
                worker,
                merge_stage: get_str(table, "merge_stage").map(str::to_string),
                max_workers: get_int(table, "max_workers").and_then(|n| u64::try_from(n).ok()),
                max_items: get_int(table, "max_items").and_then(|n| u64::try_from(n).ok()),
                on_worker_failure: get_str(table, "on_worker_failure").map(str::to_string),
            },
            has_own_layout: child(item, "context")
                .and_then(|c| c.get("regions"))
                .and_then(Item::as_table_like)
                .is_some(),
            is_terminal: transitions.is_some_and(|t| t.is_empty()),
        }
    }
}

impl ManifestDoc {
    /// Every stage's view, in file order.
    pub fn stages(&self) -> Vec<StageView> {
        self.stage_names()
            .iter()
            .filter_map(|n| self.stage(n))
            .collect()
    }

    /// Every path, grouped by the stage it leaves, in file order. A path
    /// whose `condition` the editor does not know is left out (and left
    /// alone).
    pub fn edges(&self) -> Vec<EdgeView> {
        self.stage_names()
            .into_iter()
            .filter_map(|from| {
                self.stage_item(&from)
                    .and_then(|item| child(item, "transitions"))
                    .map(|transitions| (from, transitions))
            })
            .flat_map(|(from, transitions)| {
                transitions
                    .iter()
                    .filter_map(|(to, edge)| edge_view(&from, to, edge))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// One path's view.
    pub fn edge(&self, from: &str, to: &str) -> Option<EdgeView> {
        self.stage_item(from)
            .and_then(|item| child(item, "transitions"))
            .and_then(|transitions| transitions.get(to))
            .and_then(|edge| edge_view(from, to, edge))
    }

    /// The `[context.regions]` item of a scope: the agent's, or a stage's own.
    pub(super) fn regions_item(&self, stage: Option<&str>) -> Option<&Item> {
        let parent = match stage {
            None => Some(self.doc.as_item()),
            Some(name) => self.stage_item(name),
        };
        parent
            .and_then(Item::as_table_like)
            .and_then(|p| p.get("context"))
            .and_then(Item::as_table_like)
            .and_then(|c| c.get("regions"))
            .filter(|r| r.as_table_like().is_some())
    }

    /// The regions of a scope, in document order; empty when the scope has
    /// no `regions` table.
    pub fn regions(&self, stage: Option<&str>) -> Vec<RegionView> {
        self.regions_item(stage)
            .map(|item| {
                table_keys(item)
                    .into_iter()
                    .filter_map(|name| child(item, &name).map(|table| region_view(&name, table)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One region's view.
    pub fn region(&self, stage: Option<&str>, name: &str) -> Option<RegionView> {
        self.regions_item(stage)
            .and_then(|item| child(item, name))
            .map(|table| region_view(name, table))
    }

    /// The layout a stage runs with: its own when it declares one, the
    /// agent's otherwise. `None` asks for the agent's.
    pub fn effective_regions(&self, stage: Option<&str>) -> EffectiveRegions {
        match stage {
            Some(name) if self.regions_item(Some(name)).is_some() => EffectiveRegions {
                regions: self.regions(Some(name)),
                inherited: false,
            },
            _ => EffectiveRegions {
                regions: self.regions(None),
                inherited: true,
            },
        }
    }

    /// A stage's tool routing; empty when it has none.
    pub fn tool_routing(&self, stage: &str) -> ToolRouting {
        let Some(routing) = self
            .stage_item(stage)
            .and_then(|i| child(i, "tool_routing"))
        else {
            return ToolRouting::default();
        };
        let overrides = routing
            .get("overrides")
            .and_then(Item::as_table_like)
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

    /// The stages whose tool routing (default or an override) lands in
    /// `region`: what deleting the region would break.
    pub fn stages_routing_into(&self, region: &str) -> Vec<String> {
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
    pub fn known_tools(&self) -> Vec<String> {
        let mut tools: Vec<String> = self.stages().into_iter().flat_map(|s| s.tools).collect();
        tools.sort();
        tools.dedup();
        tools
    }

    /// Every `provider/model` named by any stage, sorted and deduplicated.
    pub fn known_models(&self) -> Vec<String> {
        let mut models: Vec<String> = self.stages().into_iter().flat_map(|s| s.models).collect();
        models.sort();
        models.dedup();
        models
    }
}

/// The model chain a stage's `model` value stands for, each entry rendered as
/// `provider/model` when the blueprint pins a route and as a bare model name
/// when it leaves the route open.
///
/// Dropping the entries that name no provider is what made a migrated blueprint
/// show a single local model where it lists five: the open-route form is now
/// the ordinary one, so it has to read, not merely parse.
fn model_chain(value: Option<&Item>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    if let Some(s) = value.as_str() {
        return vec![s.to_string()];
    }
    let Some(table) = value.as_table_like() else {
        return Vec::new();
    };
    let Some(models) = table.get("models").and_then(Item::as_array) else {
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

fn edge_view(from: &str, to: &str, edge: &Item) -> Option<EdgeView> {
    edge.as_table_like()
        .and_then(|table| edge_table_view(from, to, table))
}

fn edge_table_view(from: &str, to: &str, table: &dyn TableLike) -> Option<EdgeView> {
    let hint = get_str(table, "hint").map(str::to_string);
    let kind = match get_str(table, "condition") {
        Some(c) => EdgeKind::from_condition(c)?,
        None if hint.is_some() => EdgeKind::Hint,
        None => EdgeKind::LlmChoice,
    };
    let config = table.get("transform_config").and_then(Item::as_table_like);
    let rules = TransformRules {
        carry: config.map(|c| get_strings(c, "carry")).unwrap_or_default(),
        compact: config
            .map(|c| get_strings(c, "compact"))
            .unwrap_or_default(),
        clear: config.map(|c| get_strings(c, "clear")).unwrap_or_default(),
        compact_prompt: config
            .and_then(|c| get_str(c, "compact_prompt"))
            .unwrap_or_default()
            .to_string(),
        present: config.is_some(),
    };
    Some(EdgeView {
        from: from.to_string(),
        to: to.to_string(),
        kind,
        hint,
        gated: table.contains_key("gate"),
        transform: TransformKind::parse(get_str(table, "transform").unwrap_or_default()),
        rules,
    })
}

fn region_view(name: &str, table: &dyn TableLike) -> RegionView {
    let seed = table.get("seed");
    RegionView {
        name: name.to_string(),
        kind: get_str(table, "kind").unwrap_or_default().to_string(),
        budget_percent: get_str(table, "budget").and_then(parse_percent),
        max_tokens: get_int(table, "max_tokens").and_then(|n| u64::try_from(n).ok()),
        min_tokens: get_int(table, "min_tokens").and_then(|n| u64::try_from(n).ok()),
        required: get_bool(table, "required") == Some(true),
        required_message: get_str(table, "required_message")
            .unwrap_or_default()
            .to_string(),
        seed: seed.and_then(Item::as_str).unwrap_or_default().to_string(),
        seed_is_table: seed.is_some_and(|s| s.as_table_like().is_some()),
        max_items: get_int(table, "max_items").and_then(|n| u64::try_from(n).ok()),
        strategy: get_str(table, "strategy").unwrap_or_default().to_string(),
        overflow: get_int(table, "overflow").and_then(|n| u64::try_from(n).ok()),
        description: get_str(table, "description")
            .unwrap_or_default()
            .to_string(),
    }
}

/// `"35%"` as 35.0; anything else as `None`.
pub(super) fn parse_percent(s: &str) -> Option<f64> {
    s.trim()
        .strip_suffix('%')
        .and_then(|digits| digits.trim().parse().ok())
}
