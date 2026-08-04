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
use std::path::{Path, PathBuf};

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
        let mut known_tools: HashSet<String> = leviath_tools::BuiltinTools::new(
            leviath_tools::ToolContext::new(agent_dir.to_path_buf()),
        )
        .names()
        .into_iter()
        .collect();
        known_tools.extend(leviath_tools::BuiltinTools::subagent_tool_names());

        // The agent's own `tools/`, plus the global one every agent gets.
        let dirs: Vec<PathBuf> = [Some(agent_dir.join("tools")), leviath_core::tools_dir()]
            .into_iter()
            .flatten()
            .filter(|d| d.is_dir())
            .collect();
        let (set, _skipped) = leviath_scripting::ScriptToolSet::discover(&dirs);
        known_tools.extend(set.names());

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
                .filter(|p| registry.has(p))
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

    findings.extend(lint_command_seeds(blueprint));
    findings.extend(lint_read_paths(blueprint, env));
    findings.extend(lint_safe_commands(blueprint, env));
    findings.extend(lint_held_checkpoints(blueprint));
    findings.extend(lint_graph(blueprint));

    let agent_permissions = blueprint.agent_tool_permissions();

    for stage in &blueprint.stages {
        let keys = declared.stage(&stage.name);
        findings.extend(lint_declarations(stage, keys));
        findings.extend(lint_tools(stage, env));
        findings.extend(lint_blocking_tools(stage));
        findings.extend(lint_tool_policies(stage, &agent_permissions));
        findings.extend(lint_models(stage, env));
    }

    // Worst first, stable within a severity so the order a check ran in is the
    // order its findings read in.
    findings.sort_by_key(|f| f.severity);
    findings
}

// ─── Declared keys ────────────────────────────────────────────────────────────

/// Which optional keys the manifest text actually writes, per stage, plus the
/// one agent-level block that is silently discarded.
#[derive(Debug, Default)]
struct Declared {
    /// A top-level `[model]` table exists. Nothing reads it.
    agent_model_block: bool,
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

/// Fields the stage left to a default: `mode`, `model`, and `max_iterations`.
fn lint_declarations(stage: &leviath_core::Stage, keys: StageKeys) -> Vec<LintFinding> {
    let mut findings = Vec::new();

    if !keys.mode {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "stage-missing-mode",
                "no mode is set, so the stage runs as autonomous".to_string(),
            )
            .in_stage(&stage.name)
            .with_fix("write mode = \"autonomous\" if that is what you meant"),
        );
    }

    if !keys.model {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "stage-missing-model",
                format!(
                    "no [stages.{}.model] block, so the stage runs on your \
                     configured default_provider, whatever that is",
                    stage.name
                ),
            )
            .in_stage(&stage.name)
            .with_fix(format!(
                "add model = {{ models = [{{ provider = \"...\", model = \"...\" }}] }} \
                 to [stages.{}]",
                stage.name
            )),
        );
    }

    // A fan_out stage does not run inference itself - it splits work and waits
    // on its workers - so it has no iteration count to cap.
    let counts_iterations = !matches!(stage.mode, StageMode::FanOut { .. });
    if counts_iterations && stage.max_iterations.is_none() {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "stage-missing-max-iterations",
                "no max_iterations, so the stage is unbounded unless your config \
                 sets [limits] default_max_iterations"
                    .to_string(),
            )
            .in_stage(&stage.name)
            .with_fix("give the stage a max_iterations it should never reach"),
        );
    }

    findings
}

/// Tool names that resolve to nothing, and permissions for tools the stage
/// never granted.
fn lint_tools(stage: &leviath_core::Stage, env: &LintEnv) -> Vec<LintFinding> {
    let mut findings = Vec::new();

    if !env.known_tools.is_empty() {
        for tool in &stage.available_tools {
            // `server__tool` is an MCP name. It resolves only once that server
            // is installed and connected, which is not a property of the
            // manifest, so it is never this check's business.
            if tool.contains("__") || env.known_tools.contains(tool) {
                continue;
            }
            findings.push(
                LintFinding::new(
                    LintSeverity::Error,
                    "unknown-tool",
                    format!(
                        "grants '{tool}', which is not a built-in, a sub-agent \
                         tool, or one of this agent's own tools/*.rhai"
                    ),
                )
                .in_stage(&stage.name)
                .with_fix("check the spelling, or drop the entry"),
            );
        }
    }

    let granted: HashSet<&str> = stage.available_tools.iter().map(String::as_str).collect();
    for tool in stage.tool_permissions.keys() {
        if granted.contains(tool.as_str()) {
            continue;
        }
        findings.push(
            LintFinding::new(
                LintSeverity::Error,
                "orphan-stage-permission",
                format!(
                    "sets a permission for '{tool}', which it does not grant in \
                     available_tools - it reads as a grant and is not one"
                ),
            )
            .in_stage(&stage.name)
            .with_fix(format!(
                "add '{tool}' to available_tools, or drop the permission"
            )),
        );
    }

    findings
}

/// Human-in-the-loop tools offered by a stage that runs with nobody attached.
fn lint_blocking_tools(stage: &leviath_core::Stage) -> Vec<LintFinding> {
    // Only autonomous stages are a problem: the interactive modes are where a
    // person is expected, and a fan_out stage runs no tools of its own.
    if !matches!(stage.mode, StageMode::Autonomous) || stage.allow_blocking_tools {
        return Vec::new();
    }
    stage
        .available_tools
        .iter()
        .filter(|t| BLOCKING_INTERACTION_TOOLS.contains(&canonical_tool_name(t)))
        // A tool kept in `required_tools` is the same statement of intent
        // `allow_blocking_tools` makes, made one tool at a time - and it is the
        // one that also survives an unattended run, so it is worth more.
        //
        // Canonicalised on both sides, as the runtime does: a stage granting
        // `bash` and keeping `shell` is one decision, not two.
        .filter(|t| {
            !stage
                .required_tools
                .iter()
                .any(|r| canonical_tool_name(r) == canonical_tool_name(t))
        })
        .map(|tool| {
            LintFinding::new(
                LintSeverity::Warning,
                "blocking-tool-in-autonomous-stage",
                format!(
                    "is autonomous but grants '{tool}', which suspends the run \
                     until a person answers"
                ),
            )
            .in_stage(&stage.name)
            .with_fix(
                "drop the tool, switch the stage to an interactive mode, list it in \
                 required_tools so it survives an unattended run too, or set \
                 allow_blocking_tools = true to say you meant it",
            )
        })
        .collect()
}

/// Permissions that do not land on the tool they look like they land on, and
/// shell grants left to the default.
///
/// Policy is resolved against the name the *model* calls the tool by, which is
/// always the canonical one. A permission written under an alias of a tool the
/// stage granted canonically (or the reverse) is looked up under a key nothing
/// ever asks for, so the entry has no effect at all: it reads as a decision and
/// is not one.
fn lint_tool_policies(
    stage: &leviath_core::Stage,
    agent_permissions: &HashMap<String, String>,
) -> Vec<LintFinding> {
    let has_policy = |name: &str| {
        stage.tool_permissions.contains_key(name) || agent_permissions.contains_key(name)
    };

    stage
        .available_tools
        .iter()
        .filter(|t| !has_policy(t))
        .filter_map(|tool| {
            match alias_siblings(tool).into_iter().find(|s| has_policy(s)) {
                Some(other) => Some(
                    LintFinding::new(
                        LintSeverity::Warning,
                        "permission-name-mismatch",
                        format!(
                            "grants '{tool}' but its permission is written for \
                             '{other}'. Policy is matched on the name the model \
                             calls, which is '{tool}', so that entry has no effect"
                        ),
                    )
                    .in_stage(&stage.name)
                    .with_fix(format!("rename the permission key '{other}' to '{tool}'")),
                ),
                // No policy under any spelling. Only worth saying for the shell,
                // whose default is `ask` - and an `ask` with nobody to answer
                // waits rather than denying, so an unattended run hangs on the
                // first command instead of failing it.
                None if canonical_tool_name(tool) == "shell" => Some(
                    LintFinding::new(
                        LintSeverity::Warning,
                        "implicit-shell-policy",
                        format!(
                            "grants '{tool}' with no permission set for it, so it \
                             defaults to ask - and an unattended run waits on that \
                             prompt rather than being denied"
                        ),
                    )
                    .in_stage(&stage.name)
                    .with_fix(format!(
                        "set {tool} = \"allow\" or \"deny\" in [tool_permissions] or \
                         [stages.{}.tool_permissions]",
                        stage.name
                    )),
                ),
                None => None,
            }
        })
        .collect()
}

/// Every other name for the same built-in tool: the canonical name when `name`
/// is an alias, plus every alias of it. Never includes `name` itself.
fn alias_siblings(name: &str) -> Vec<String> {
    let canonical = canonical_tool_name(name);
    std::iter::once(canonical)
        .chain(
            leviath_tools::TOOL_ALIASES
                .iter()
                .filter(|(_, c)| *c == canonical)
                .map(|(alias, _)| *alias),
        )
        .filter(|s| *s != name)
        .map(str::to_string)
        .collect()
}

/// Regions whose `seed = { command = "..." }` runs a shell command at spawn.
///
/// This one is an audit line rather than a complaint: the commands run before
/// the first inference and before any tool-approval prompt, so whoever is about
/// to `lev add` a blueprint they did not write should see them first.
fn lint_command_seeds(blueprint: &Blueprint) -> Vec<LintFinding> {
    let seeds: Vec<String> = blueprint
        .context_layout
        .regions
        .iter()
        .filter_map(|r| match &r.seed {
            Some(leviath_core::layout::RegionSeed::Command { command }) => {
                Some(format!("{}: {command}", r.name))
            }
            _ => None,
        })
        .collect();
    if seeds.is_empty() {
        return Vec::new();
    }
    vec![
        LintFinding::new(
            LintSeverity::Note,
            "command-seed",
            format!(
                "{} region(s) run a shell command at spawn, before the first \
                 inference and before any tool-approval prompt: {}",
                seeds.len(),
                seeds.join(", ")
            ),
        )
        .with_fix(
            "disable with `--no-seed-commands`, or machine-wide via \
             `[security] allow_seed_commands = false`",
        ),
    ]
}

/// `[read_paths]` declarations: what the agent asks to read beyond its workdir,
/// whether this machine's config actually grants each entry, and a sharper
/// warning for an entry so broad it amounts to "my whole home directory" or
/// "any absolute path".
///
/// The grant status is the point (issue #209). A declaration is inert on its
/// own, and before this it took reading the config schema to find that out: the
/// blueprint validated, the run spawned, and the first out-of-workdir read was
/// refused with nothing said earlier. When `env` has no answer - the daemon's
/// offline lint, which has no user config to consult - the note falls back to
/// stating the rule.
/// Checkpoints that hold a `--yolo` run for a person.
///
/// A note, not a warning: this is the blueprint working as written. It is worth
/// saying because `--yolo` reads as "run without me", so a run that stops anyway
/// looks like a hang, and `lev validate` is where an author or an operator finds
/// out before the run rather than during it.
fn lint_held_checkpoints(blueprint: &Blueprint) -> Vec<LintFinding> {
    let held = crate::held_checkpoints::held_points(blueprint)
        .into_iter()
        .map(|h| (h, "an interaction point that declares unattended = \"ask\""))
        .chain(
            crate::held_checkpoints::held_tools(blueprint)
                .into_iter()
                .map(|h| (h, "a blocking tool the stage keeps in required_tools")),
        );
    held.map(|(h, what)| {
        LintFinding::new(
            LintSeverity::Note,
            "holds-under-yolo",
            format!(
                "'{}' still stops an unattended run for a person: {what}",
                h.name
            ),
        )
        .in_stage(&h.stage)
        .with_fix(
            "this is deliberate if the checkpoint needs a person. `[limits] \
             interaction_timeout_secs` bounds the wait, and an unanswered \
             checkpoint stops the run with an error rather than approving it",
        )
    })
    .collect()
}

/// A `[safe_commands]` block: entries that will never match, and whether the
/// block counts at all on this install.
fn lint_safe_commands(blueprint: &Blueprint, env: &LintEnv) -> Vec<LintFinding> {
    let Some(sc) = blueprint.safe_commands.as_ref() else {
        return Vec::new();
    };
    let mut findings = Vec::new();

    // An entry the key parser reads as anything other than itself can never
    // match a call, so it reads as a decision and is not one. An error rather
    // than a warning: unlike a permission written under an alias, there is no
    // spelling of this that does something else useful.
    for entry in &sc.shell {
        if !crate::shell_keys::is_valid_prefix(entry) {
            findings.push(
                LintFinding::new(
                    LintSeverity::Error,
                    "unparseable-safe-command",
                    format!(
                        "[safe_commands] shell entry '{entry}' is not a bare command prefix, so \
                         no call can ever match it"
                    ),
                )
                .with_fix(
                    "write a program, optionally with the subcommand that narrows it: \
                     'rg', 'cargo test', 'git status'. No flags, arguments, redirects, \
                     quotes or chained commands",
                ),
            );
        }
    }

    // Declaring is not granting, and an author who does not know that ships a
    // block that does nothing on every install but their own.
    if !sc.shell.is_empty() || !sc.tools.is_empty() {
        let message = match env.safe_commands_granted {
            Some(true) => None,
            Some(false) => Some(
                "declares [safe_commands], and your config does not honour blueprint \
                 safe-commands, so none of it applies"
                    .to_string(),
            ),
            None => Some(
                "declares [safe_commands]. Declaring is not granting: entries apply only \
                 where the user opts in"
                    .to_string(),
            ),
        };
        if let Some(message) = message {
            findings.push(
                LintFinding::new(LintSeverity::Note, "safe-commands-declared", message).with_fix(
                    format!(
                        "set [agent_safe_commands.{}] allow_blueprint = true in your \
                         config.toml, or [security] allow_blueprint_safe_commands for every \
                         agent",
                        blueprint.name
                    ),
                ),
            );
        }
    }

    findings
}

fn lint_read_paths(blueprint: &Blueprint, env: &LintEnv) -> Vec<LintFinding> {
    let Some(rp) = blueprint
        .read_paths
        .as_ref()
        .filter(|rp| !rp.allow.is_empty())
    else {
        return Vec::new();
    };
    let mut findings = match &env.read_paths {
        Some(Ok(report)) => grant_findings(report),
        // A grant list of the user's own that will not compile is a hard spawn
        // error; saying so here is where it costs least.
        Some(Err(e)) => vec![
            LintFinding::new(LintSeverity::Warning, "read-paths-grant-invalid", e.clone())
                .with_fix("fix the entry in your config.toml, or remove it"),
        ],
        None => vec![
            LintFinding::new(
                LintSeverity::Note,
                "read-paths-declared",
                format!(
                    "declares [read_paths] (reads outside the run workdir): {}",
                    rp.allow.join(", ")
                ),
            )
            .with_fix("these are refused unless your own config grants them"),
        ],
    };
    findings.extend(
        rp.allow
            .iter()
            .filter(|e| read_path_entry_is_broad(e))
            .map(|entry| {
                LintFinding::new(
                    LintSeverity::Warning,
                    "broad-read-path",
                    format!(
                        "read_paths entry '{entry}' is very broad - it can match \
                     your entire home directory or any path on this machine"
                    ),
                )
                .with_fix("name the directory it actually needs")
            }),
    );
    findings
}

/// One finding per declared entry, judged against the config: a note for the
/// ones that are live, a warning naming each one that is not, and the stanza
/// that would grant them all.
///
/// An entry whose pattern admits no representative path is reported as
/// unchecked rather than as inert - claiming a working grant is broken would be
/// worse than saying nothing.
fn grant_findings(report: &crate::read_path_report::GrantReport) -> Vec<LintFinding> {
    let mut findings = vec![
        LintFinding::new(
            LintSeverity::Note,
            "read-paths-declared",
            format!(
                "declares [read_paths] (reads outside the run workdir): {}",
                report.summary()
            ),
        )
        .with_fix(match report.allow_blueprint {
            true => "all granted by [security] allow_blueprint_read_paths = true".to_string(),
            false => report
                .entries
                .iter()
                .map(|e| format!("{}: {}", e.raw, e.status.label()))
                .collect::<Vec<_>>()
                .join("; "),
        }),
    ];
    if report.has_ungranted() {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "read-paths-not-granted",
                format!(
                    "your config does not grant {}: reads matching them will be refused",
                    report.ungranted().join(", ")
                ),
            )
            .with_fix(format!(
                "add to your config.toml: {}",
                report.grant_stanza().join(" ")
            )),
        );
    }
    findings
}

/// Whether a `[read_paths]` entry grants effectively unlimited read access:
/// the home directory itself, a filesystem root, or a pattern whose first
/// component already matches anything.
fn read_path_entry_is_broad(entry: &str) -> bool {
    let pattern = entry
        .strip_prefix("glob:")
        .or_else(|| entry.strip_prefix("regex:"))
        .unwrap_or(entry);
    let pattern = pattern.replace('\\', "/");
    let trimmed = pattern.trim_end_matches('/');
    matches!(trimmed, "~" | "")
        || trimmed == "/**"
        || pattern.starts_with("**")
        || pattern.starts_with("/.*")
        || trimmed == "/.+"
}

/// Graph shape: stages the entry can never reach, and cycles with no revisit
/// cap. Both only mean anything for a blueprint that declares transitions at
/// all - a linear one has no graph to walk.
fn lint_graph(blueprint: &Blueprint) -> Vec<LintFinding> {
    if !blueprint.stages.iter().any(|s| s.transitions.is_some()) {
        return Vec::new();
    }
    let stage_names: HashSet<&str> = blueprint.stages.iter().map(|s| s.name.as_str()).collect();
    let entry = blueprint.resolve_entry_stage_name();

    // Breadth-first from the entry stage; whatever is left over is orphaned.
    let mut reachable = HashSet::new();
    let mut queue = std::collections::VecDeque::from([entry.clone()]);
    while let Some(name) = queue.pop_front() {
        if !reachable.insert(name.clone()) {
            continue;
        }
        let Some(stage) = blueprint.find_stage(&name) else {
            continue;
        };
        // A fan_out stage reaches its worker and merge stages through its own
        // config rather than a transition edge, so following only `transitions`
        // would report a perfectly wired worker as an orphan.
        let fan_out = match &stage.mode {
            StageMode::FanOut { config } => [
                config.worker_stage.as_deref(),
                config.merge_stage.as_deref(),
            ],
            _ => [None, None],
        };
        let edges = stage
            .transitions
            .iter()
            .flat_map(|t| t.keys().map(String::as_str))
            .chain(fan_out.into_iter().flatten());
        for target in edges {
            if !reachable.contains(target) && stage_names.contains(target) {
                queue.push_back(target.to_string());
            }
        }
    }

    let mut findings: Vec<LintFinding> = blueprint
        .stages
        .iter()
        .filter(|s| !reachable.contains(s.name.as_str()))
        .map(|s| {
            LintFinding::new(
                LintSeverity::Warning,
                "unreachable-stage",
                format!("cannot be reached from entry stage '{entry}'"),
            )
            .in_stage(&s.name)
            .with_fix("give some stage a transition to it, or delete it")
        })
        .collect();

    // A pair of stages that each transition to the other, where the one being
    // returned to has no revisit cap, can bounce forever.
    for stage in &blueprint.stages {
        let Some(transitions) = &stage.transitions else {
            continue;
        };
        for target in transitions.keys().filter(|t| **t != stage.name) {
            let Some(target_stage) = blueprint.find_stage(target) else {
                continue;
            };
            let Some(t2) = &target_stage.transitions else {
                continue;
            };
            if t2.contains_key(&stage.name) && target_stage.max_revisits.is_none() {
                findings.push(
                    LintFinding::new(
                        LintSeverity::Warning,
                        "cycle-without-max-revisits",
                        format!(
                            "is in a cycle with '{}' and has no max_revisits",
                            stage.name
                        ),
                    )
                    .in_stage(target)
                    .with_fix("set max_revisits so the loop has to end"),
                );
            }
        }
    }

    findings
}

/// Models and providers the install cannot resolve.
fn lint_models(stage: &leviath_core::Stage, env: &LintEnv) -> Vec<LintFinding> {
    let mut findings = Vec::new();

    for entry in &stage.model.models {
        // A provider with no catalog here is open-ended (Ollama serves whatever
        // is pulled, OpenRouter's list runs to hundreds, a script provider
        // defines its own). Checking a model against a catalog that does not
        // claim to be complete would only produce false alarms.
        let catalog_known = env.known_models.iter().any(|(p, _)| *p == entry.provider);
        let listed = env
            .known_models
            .iter()
            .any(|(p, m)| *p == entry.provider && *m == entry.model);
        if catalog_known && !listed {
            findings.push(
                LintFinding::new(
                    LintSeverity::Warning,
                    "unknown-model",
                    format!(
                        "names {}/{}, which is not a model this build knows about",
                        entry.provider, entry.model
                    ),
                )
                .in_stage(&stage.name)
                .with_fix(
                    "check `lev models list`, or `lev models list --remote` \
                           if it is newer than this build",
                ),
            );
        }
    }

    // Reported per stage, not per entry: the models list is an ordered set of
    // fallbacks, so naming a provider this install cannot reach is normal and
    // expected as long as something later in the list answers. What is worth
    // saying is that *nothing* in the list does, which is the shape that
    // reaches the runtime as "no usable provider" at spawn.
    if let Some(available) = &env.available_providers
        && !stage.model.models.is_empty()
        && !stage
            .model
            .models
            .iter()
            .any(|e| available.contains(&e.provider))
    {
        let tried: Vec<&str> = stage
            .model
            .models
            .iter()
            .map(|e| e.provider.as_str())
            .collect();
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "no-reachable-provider",
                format!(
                    "names no provider this install can reach (tried {}), so it \
                     falls back to your default model",
                    tried.join(", ")
                ),
            )
            .in_stage(&stage.name)
            .with_fix("run `lev setup` to configure one of them, or add a provider you have"),
        );
    }

    findings
}

#[cfg(test)]
mod tests;
