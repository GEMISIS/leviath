//! Blueprint lint: the checks [`Blueprint::validate`] deliberately does not make.
//!
//! `Blueprint::validate` answers "is this manifest structurally coherent" - the
//! layout fits, the graph resolves, fan-out wiring points at real stages. It
//! says nothing about the fields whose *absence* quietly changes what a run
//! does, and those are what actually bite:
//!
//! - a stage with no `[stages.<name>.model]` table parses fine, because the
//!   parser substitutes a default, and then runs on whatever the user's default
//!   provider happens to be
//! - an agent-level `[model]` block is never read at all, so the author's model
//!   choice is discarded silently
//! - a typo in `available_tools` matches nothing, and the stage just advertises
//!   one tool fewer - the model is told the tool does not exist
//! - an autonomous stage granting `ask_user_text` parks in `WaitingInput` the
//!   first time it asks, with nobody there to answer
//!
//! Each of those is invisible on inspection and shows up hours later as a stuck
//! run. This module names them at author time instead.
//!
//! Questions about what the author *declared* ("is there a `mode` key?") are
//! answered from the manifest text, not from the parsed [`Blueprint`]: by then
//! the parser has already filled in its defaults, and asking the struct cannot
//! tell "wrote `autonomous`" apart from "wrote nothing".
//!
//! [`Blueprint::validate`]: leviath_core::Blueprint::validate

use std::collections::{HashMap, HashSet};
use std::path::Path;

use leviath_core::Blueprint;
use leviath_core::blueprint::StageMode;
use leviath_runtime::dynamic_interaction::BLOCKING_INTERACTION_TOOLS;
use leviath_tools::canonical_tool_name;
use serde::{Deserialize, Serialize};

/// How much a finding matters. Only [`LintSeverity::Error`] fails
/// `lev validate`; warnings are printed and the command still exits zero
/// (unless `--deny-warnings` is passed); notes never fail anything.
///
/// Declared worst-first so sorting by it groups the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LintSeverity {
    /// The manifest says something that cannot be what the author meant - a
    /// tool name matching nothing, a permission for a tool the stage never
    /// granted.
    Error,
    /// The manifest leaves a decision to a default the author may not know
    /// about.
    Warning,
    /// Nothing is wrong; the blueprint is doing something worth knowing before
    /// you run it, like reaching outside its workdir or running a shell command
    /// at spawn. A note must never fail a build, so `--deny-warnings` skips it.
    Note,
}

impl LintSeverity {
    /// Fixed-width label for the report, so the messages line up.
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "ERR ",
            Self::Warning => "WARN",
            Self::Note => "NOTE",
        }
    }
}

/// One thing worth telling the author about.
///
/// Serialize only: `code` is a `&'static str` pointing at a literal in this
/// file, which no deserializer can produce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LintFinding {
    /// How much this matters, and therefore whether it fails the check.
    pub severity: LintSeverity,
    /// Stable slug (`"unknown-tool"`), so a finding can be referenced in an
    /// issue or grepped for in daemon logs without quoting prose.
    pub code: &'static str,
    /// The stage it belongs to, when it belongs to one.
    pub stage: Option<String>,
    /// What is wrong.
    pub message: String,
    /// What to do about it. Rendered on its own indented line.
    pub fix: Option<String>,
}

impl LintFinding {
    fn new(severity: LintSeverity, code: &'static str, message: String) -> Self {
        Self {
            severity,
            code,
            stage: None,
            message,
            fix: None,
        }
    }

    fn in_stage(mut self, stage: &str) -> Self {
        self.stage = Some(stage.to_string());
        self
    }

    fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    /// Whether this finding should fail the command.
    pub fn is_error(&self) -> bool {
        self.severity == LintSeverity::Error
    }

    /// One-line rendering for a log record: `stage 'x': message`.
    pub fn one_line(&self) -> String {
        match &self.stage {
            Some(stage) => format!("stage '{stage}': {}", self.message),
            None => self.message.clone(),
        }
    }
}

/// Facts about the machine the blueprint will run on, which the manifest alone
/// cannot supply.
///
/// Every field is "unknown" when empty/`None`, and an unknown field skips its
/// check entirely rather than guessing. A linter that cannot see the installed
/// MCP servers must not claim their tools do not exist.
#[derive(Debug, Default, Clone)]
pub struct LintEnv {
    /// Every tool name a manifest may legally write: canonical built-ins, their
    /// aliases, the sub-agent tools, this agent's own `tools/*.rhai`, and any
    /// MCP tools already resolved. Empty skips the unknown-tool check.
    pub known_tools: HashSet<String>,

    /// `(provider, model)` rows for providers whose catalog is closed enough to
    /// check against. A provider with no row here is not checked at all, which
    /// is what keeps open catalogs (Ollama, OpenRouter, script providers) from
    /// producing noise.
    pub known_models: Vec<(String, String)>,

    /// The providers the blueprint names that this install can actually reach,
    /// as answered by `ProviderRegistry::has`. `None` means nobody asked, so
    /// the check is skipped. Resolution lives with the caller because script
    /// providers are loaded on demand and cannot be enumerated up front.
    pub available_providers: Option<HashSet<String>>,

    /// Which of the blueprint's `[read_paths]` this install's config grants.
    /// `None` means nobody asked (the daemon's offline lint), in which case the
    /// check only says that a declaration needs granting. `Some(Err(..))` is a
    /// grant list of the user's own that will not compile.
    pub read_paths: Option<Result<crate::read_path_report::GrantReport, String>>,

    /// Whether this install's config honours the blueprint's own
    /// `[safe_commands]`. `None` means nobody asked (the daemon's offline
    /// lint), in which case the check only says the declaration needs granting.
    ///
    /// A bool rather than a report: unlike read paths, where *which* entries are
    /// granted is the interesting part, a safe-commands block is honoured whole
    /// or not at all.
    pub safe_commands_granted: Option<bool>,
}

impl LintEnv {
    /// Everything that can be known without touching the user's config: the
    /// built-in tools (aliases included), the sub-agent tools, the script tools
    /// in `agent_dir/tools` and the global tools directory, and the model
    /// catalogs this build ships.
    ///
    /// This is what the daemon lints against at spawn. It deliberately leaves
    /// `available_providers` unset: the daemon already fails a spawn outright
    /// when no listed provider is registered, so re-deriving that here would
    /// cost a registry build per agent to say something the spawn will say
    /// louder a moment later.
    pub fn offline(agent_dir: &Path) -> Self {
        // The four discovery rules live in `tool_inventory` rather than here,
        // because `GET /api/tools` has to answer the same question and two
        // copies of "where does a tool come from" would not have stayed equal.
        // The lint wants only the names; the endpoint wants the sources too.
        let known_tools =
            crate::tool_inventory::ToolInventory::discover(Some(agent_dir), None).names();

        Self {
            known_tools,
            known_models: crate::commands::models::closed_catalog_models(),
            available_providers: None,
            read_paths: None,
            safe_commands_granted: None,
        }
    }

    /// Add the answer to "can this install reach the providers the blueprint
    /// names", asked of the same registry the runtime resolves stages against
    /// so a script provider counts exactly when it would really load.
    pub fn with_providers(mut self, blueprint: &Blueprint, config: &crate::config::Config) -> Self {
        let registry = crate::commands::run::build_provider_registry_from_config(config);
        self.available_providers = Some(
            blueprint
                .stages
                .iter()
                .flat_map(|s| s.model.models.iter())
                .map(|e| e.provider.clone())
                .filter(|p| registry.as_ref().is_ok_and(|r| r.has(p)))
                .collect(),
        );
        self
    }

    /// Add the answer to "does this install's config grant what the blueprint
    /// declares under `[read_paths]`", per entry.
    ///
    /// Separate from [`Self::with_providers`] because it needs a workdir:
    /// relative entries resolve against the one a run would use, which for a
    /// command run outside a run is the directory it was invoked from.
    pub fn with_read_paths(
        mut self,
        blueprint: &Blueprint,
        config: &crate::config::Config,
        workdir: &Path,
    ) -> Self {
        self.read_paths = crate::read_path_report::build(blueprint, config, workdir);
        // Asked here rather than in its own builder: both answers come from the
        // same config, and a caller that has one always has the other.
        self.safe_commands_granted = Some(
            config.security.allow_blueprint_safe_commands
                || config
                    .agent_safe_commands
                    .get(&blueprint.name)
                    .is_some_and(|a| a.allow_blueprint),
        );
        self
    }
}

/// Lint `blueprint`, which was parsed from `content`.
///
/// The two arguments describe the same manifest: `blueprint` for what the
/// engine will do with it, `content` for what the author actually wrote.
pub fn lint_manifest(content: &str, blueprint: &Blueprint, env: &LintEnv) -> Vec<LintFinding> {
    let declared = Declared::from_text(content);
    let mut findings = Vec::new();

    if declared.agent_model_block {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "agent-model-block-ignored",
                "the top-level [model] block is not read by anything: model \
                 selection is per stage"
                    .to_string(),
            )
            .with_fix("move it into each [stages.<name>.model] that needs it"),
        );
    }

    findings.extend(lint_dropped_seeds(&declared, blueprint));
    findings.extend(lint_command_seeds(blueprint));
    findings.extend(lint_read_paths(blueprint, env));
    findings.extend(lint_safe_commands(blueprint, env));
    findings.extend(lint_held_checkpoints(blueprint));
    findings.extend(lint_graph(blueprint));
    findings.extend(lint_output_reachable(blueprint));
    findings.extend(lint_dead_end_possible(blueprint));
    findings.extend(lint_compacted_deliverables(blueprint));

    let agent_permissions = blueprint.agent_tool_permissions();

    for stage in &blueprint.stages {
        let keys = declared.stage(&stage.name);
        findings.extend(lint_declarations(stage, keys));
        findings.extend(lint_tools(stage, env));
        findings.extend(lint_blocking_tools(stage));
        findings.extend(lint_tool_policies(stage, &agent_permissions));
        findings.extend(lint_models(stage, env));
        findings.extend(lint_output_stage(stage));
    }

    // Worst first, stable within a severity so the order a check ran in is the
    // order its findings read in.
    findings.sort_by_key(|f| f.severity);
    findings
}

/// A region wrote a `seed` the parser could not read, so it has none.
///
/// `parse_region_seed` returns `None` for a seed table with no recognized key
/// and for a seed that is neither a string nor a table, and the region then
/// simply starts empty. That is deliberate - an unknown key is not worth
/// rejecting a whole manifest over - but it is invisible, and a one-character
/// typo (`caller_input` for `caller`) reads exactly like a working blueprint
/// until an agent answers a question it was never given. This is the check that
/// says so.
fn lint_dropped_seeds(declared: &Declared, blueprint: &Blueprint) -> Vec<LintFinding> {
    declared
        .seeded_regions
        .iter()
        .filter(|name| {
            blueprint
                .context_layout
                .get_region(name)
                .is_some_and(|r| r.seed.is_none())
        })
        .map(|name| {
            LintFinding::new(
                LintSeverity::Warning,
                "region-seed-not-understood",
                format!(
                    "region '{name}' declares a seed that isn't one of the \
                     recognized forms, so it is ignored and the region starts empty"
                ),
            )
            .with_fix(
                "use a string (the caller input key), or one of \
                 { caller = }, { literal = }, { files = }, { glob = }, \
                 { rhai = }, { command = }",
            )
        })
        .collect()
}

// ─── Declared keys ────────────────────────────────────────────────────────────

/// Which optional keys the manifest text actually writes, per stage, plus the
/// one agent-level block that is silently discarded.
#[derive(Debug, Default)]
struct Declared {
    /// A top-level `[model]` table exists. Nothing reads it.
    agent_model_block: bool,
    /// Regions whose text writes a `seed` key, whatever its shape. Compared
    /// against the parsed seed to catch the ones the parser threw away.
    seeded_regions: Vec<String>,
    /// Per stage name, the keys that stage wrote.
    stages: HashMap<String, StageKeys>,
    /// The manifest text could not be re-read. Every key is then reported as
    /// declared, so an unreadable manifest produces no declaration warnings
    /// rather than a full set of false ones.
    opaque: bool,
}

#[derive(Debug, Default, Clone, Copy)]
struct StageKeys {
    mode: bool,
    model: bool,
}

impl Declared {
    fn from_text(content: &str) -> Self {
        // `toml::from_str` and not `str::parse`: the latter deserializes a bare
        // TOML *value*, not a document, and rejects every real manifest.
        let Ok(root) = toml::from_str::<toml::Table>(content) else {
            return Self {
                opaque: true,
                ..Self::default()
            };
        };
        let agent_model_block = root.get("model").is_some_and(toml::Value::is_table);
        // Both region spellings - inline `name = { seed = ... }` under
        // `[context.regions]` and a `[context.regions.name]` section - land here
        // as the same nested table, so one path covers both.
        let seeded_regions = root
            .get("context")
            .and_then(toml::Value::as_table)
            .and_then(|c| c.get("regions"))
            .and_then(toml::Value::as_table)
            .map(|regions| {
                regions
                    .iter()
                    .filter(|(_, body)| body.get("seed").is_some())
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let stages = root
            .get("stages")
            .and_then(toml::Value::as_table)
            .map(|t| {
                t.iter()
                    .map(|(name, body)| {
                        (
                            name.clone(),
                            StageKeys {
                                mode: body.get("mode").is_some(),
                                model: body.get("model").is_some(),
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            agent_model_block,
            seeded_regions,
            stages,
            opaque: false,
        }
    }

    /// What `stage` declared. An unreadable manifest, or a stage the text has
    /// no entry for, reports everything as declared so nothing is warned about.
    fn stage(&self, stage: &str) -> StageKeys {
        if self.opaque {
            return StageKeys {
                mode: true,
                model: true,
            };
        }
        self.stages.get(stage).copied().unwrap_or(StageKeys {
            mode: true,
            model: true,
        })
    }
}

// ─── Checks ───────────────────────────────────────────────────────────────────

// The checks themselves, one module per question they answer. Imported rather
// than re-exported: `lint_manifest` is the only caller and the only entry point
// anyone outside this module needs, so the individual checks stay internal.
mod checks;
use checks::*;
mod security;
use security::*;

#[cfg(test)]
mod tests;
