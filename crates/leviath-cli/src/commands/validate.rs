//! `lev validate` - Validate an agent blueprint.

use clap::Args;
use std::path::PathBuf;

use crate::lint::{LintEnv, LintFinding, LintSeverity, lint_manifest};

/// Arguments for `lev validate`.
#[derive(Args)]
pub struct ValidateArgs {
    /// Path to the agent directory or agent.leviath file
    #[arg(default_value = ".")]
    pub(crate) path: String,

    /// Fail on warnings too, not only errors. Notes never fail.
    #[arg(long)]
    pub(crate) deny_warnings: bool,

    /// Report the blueprint and every finding as JSON instead of prose. The
    /// exit status is unchanged, so a caller can branch on either.
    #[arg(long)]
    pub(crate) json: bool,

    /// Draw the stage graph after the report: the same picture the
    /// dashboard's stage explorer shows, as plain text. Ignored with --json.
    #[arg(long)]
    pub(crate) graph: bool,

    /// How many columns the graph may use; a wider one is shrunk to fit.
    #[arg(long, default_value_t = 120, requires = "graph")]
    pub(crate) width: u16,
}

/// The blueprint itself, for a caller that wants to know what it just validated.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BlueprintSummary {
    /// The blueprint's `[agent] name`.
    pub name: String,
    /// Its declared version.
    pub version: String,
    /// Its one-line description.
    pub description: String,
    /// Null when the manifest names no `entry_stage`, in which case the first
    /// stage is the entry.
    pub entry_stage: Option<String>,
    /// Stage names in blueprint order.
    pub stages: Vec<String>,
    /// Whether `lev run <agent> --task <text>` is accepted. False means a run
    /// handing this agent a task is refused at spawn, so a harness can check
    /// here instead of discovering it from the run-time error (issue #414).
    pub accepts_task: bool,
    /// Every caller-settable input, in declaration order: which flag seeds
    /// which region, and whether a run can start without it.
    pub inputs: Vec<InputSummary>,
}

/// One caller-settable input: a `--<key>` flag on `lev run` (equally the
/// `regions.<key>` field over the API, or an ACP `---region:<key>---` block)
/// and the region its value seeds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InputSummary {
    /// The caller key. `task` is the `--task` flag; anything else is a
    /// blueprint-defined `--<key>` flag.
    pub key: String,
    /// The region the value lands in. Often the same as `key`, but a seed may
    /// name a shorter key for a longer region (`criteria` for
    /// `review_criteria`).
    pub region: String,
    /// True when the region is required, so a spawn without this input fails.
    pub required: bool,
}

/// The caller-settable inputs a blueprint declares, in declaration order.
///
/// The prose and JSON halves of the report both read from this one walk, so
/// they cannot disagree about what the agent takes.
fn input_summaries(blueprint: &leviath_core::Blueprint) -> Vec<InputSummary> {
    blueprint
        .context_layout
        .regions
        .iter()
        .filter_map(|r| match &r.seed {
            Some(leviath_core::layout::RegionSeed::CallerInput { name }) => Some(InputSummary {
                key: name.clone(),
                region: r.name.clone(),
                required: r.required,
            }),
            _ => None,
        })
        .collect()
}

/// The "Inputs:" lines `print_success` shows, answering at validate time what
/// `lev run` would otherwise only reveal by refusing at spawn (issue #414):
/// which flags this agent takes, and explicitly that `--task` is not among
/// them when no region is seeded from the task.
fn input_lines(blueprint: &leviath_core::Blueprint) -> Vec<String> {
    let inputs = input_summaries(blueprint);
    if inputs.is_empty() {
        return vec![
            "  Inputs: none - this agent takes no --task or other caller input".to_string(),
        ];
    }
    let flags: Vec<String> = inputs
        .iter()
        .map(|i| {
            let mut flag = format!("--{}", i.key);
            let mut notes = Vec::new();
            if i.required {
                notes.push("required".to_string());
            }
            if i.key != i.region {
                notes.push(format!("seeds region '{}'", i.region));
            }
            if !notes.is_empty() {
                flag.push_str(&format!(" ({})", notes.join(", ")));
            }
            flag
        })
        .collect();
    let mut lines = vec![format!("  Inputs: {}", flags.join(", "))];
    if !blueprint.accepts_task() {
        lines.push(format!(
            "  Note: this agent takes no --task; give it input via {}",
            inputs
                .iter()
                .map(|i| format!("--{}", i.key))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines
}

/// What `lev validate --json` prints.
///
/// One shape for every outcome, so a caller parses once and branches on
/// `valid`. A manifest that did not parse fills `error` and leaves `blueprint`
/// null; one that did fills `blueprint` and leaves `error` null. `code` on each
/// finding is a stable slug to branch on, where the prose line is written to be
/// read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ValidateReport {
    /// True when nothing would have failed the command.
    pub valid: bool,
    /// Present when the manifest parsed and validated.
    pub blueprint: Option<BlueprintSummary>,
    /// Present when it did not.
    pub error: Option<String>,
    /// Everything the lint had to say, at every severity.
    pub findings: Vec<LintFinding>,
    /// How many findings are errors. Non-zero means the blueprint will not run.
    pub errors: usize,
    /// How many are warnings: it runs, but something looks wrong.
    pub warnings: usize,
    /// How many are notes: things worth seeing that are not problems.
    pub notes: usize,
}

impl ValidateReport {
    /// The report for a manifest that got as far as linting.
    fn linted(
        blueprint: &leviath_core::Blueprint,
        findings: Vec<LintFinding>,
        deny_warnings: bool,
    ) -> Self {
        let count = |want: LintSeverity| findings.iter().filter(|f| f.severity == want).count();
        let (errors, warnings) = (count(LintSeverity::Error), count(LintSeverity::Warning));
        Self {
            // Mirrors the exit-status rule exactly: notes never fail a build.
            valid: errors == 0 && !(deny_warnings && warnings > 0),
            blueprint: Some(BlueprintSummary {
                name: blueprint.name.clone(),
                version: blueprint.version.clone(),
                description: blueprint.description.clone(),
                entry_stage: blueprint.entry_stage.clone(),
                stages: blueprint.stages.iter().map(|s| s.name.clone()).collect(),
                accepts_task: blueprint.accepts_task(),
                inputs: input_summaries(blueprint),
            }),
            error: None,
            errors,
            warnings,
            notes: count(LintSeverity::Note),
            findings,
        }
    }

    /// The report for a manifest that never parsed or never validated.
    fn failed(error: String) -> Self {
        Self {
            valid: false,
            blueprint: None,
            error: Some(error),
            findings: Vec::new(),
            errors: 1,
            warnings: 0,
            notes: 0,
        }
    }

    fn print(&self) {
        // Owned scalars and vectors with no map keys to reject, so this cannot
        // fail.
        println!(
            "{}",
            serde_json::to_string_pretty(self).expect("a validate report serializes")
        );
    }
}

/// Resolve, read, parse, and validate the manifest at `path`. Distinguishes
/// I/O failures (propagated as a normal error) from parse/validation
/// failures (which `execute()` reports specially and exits(1) on) so the
/// core logic can be unit tested without killing the test process.
#[derive(Debug)]
enum ManifestCheckError {
    Io(anyhow::Error),
    Parse(String),
    Validation(String),
}

/// A manifest that parsed and validated, kept alongside the text it came from
/// so the linter can ask what the author actually wrote.
#[derive(Debug)]
struct CheckedManifest {
    blueprint: leviath_core::Blueprint,
    content: String,
    /// The directory holding the manifest: where its `tools/` live.
    agent_dir: PathBuf,
}

/// The manifest a validate target names: the file itself, or the `agent.leviath`
/// inside a directory.
///
/// Pure, and shared by [`check_manifest`] and the stale-install suffix, so both
/// resolve a target the same way. Says nothing about whether the file exists.
fn manifest_path_for(path: &std::path::Path) -> std::path::PathBuf {
    if path.is_file() {
        path.to_path_buf()
    } else {
        path.join("agent.leviath")
    }
}

fn check_manifest(path: &std::path::Path) -> Result<CheckedManifest, ManifestCheckError> {
    let manifest_path = manifest_path_for(path);
    if !manifest_path.exists() {
        return Err(ManifestCheckError::Io(anyhow::anyhow!(
            "No agent.leviath found at {}",
            path.display()
        )));
    }

    let content = std::fs::read_to_string(&manifest_path).map_err(|e| {
        ManifestCheckError::Io(anyhow::anyhow!(
            "Failed to read {}: {}",
            manifest_path.display(),
            e
        ))
    })?;

    let blueprint = leviath_core::manifest::parse_manifest(&content)
        .map_err(|e| ManifestCheckError::Parse(e.to_string()))?;

    blueprint
        .validate()
        .map_err(|e| ManifestCheckError::Validation(e.to_string()))?;

    // Custom regions' Rhai scripts must resolve to readable, compilable
    // files with a well-formed `fn render(ctx)` - the same check a spawn
    // performs, surfaced here where a typo'd path or syntax error is cheap
    // to find.
    crate::daemon::spawn::resolve_region_scripts(&blueprint, &manifest_path.to_string_lossy())
        .map_err(ManifestCheckError::Validation)?;

    let agent_dir = manifest_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    Ok(CheckedManifest {
        blueprint,
        content,
        agent_dir,
    })
}

/// Print the "valid blueprint" summary + non-fatal warnings.
fn print_success(blueprint: &leviath_core::Blueprint) {
    println!("✓ Blueprint '{}' is valid.", blueprint.name);
    println!(
        "  {} stages, version {}",
        blueprint.stages.len(),
        blueprint.version
    );
    for line in input_lines(blueprint) {
        println!("{line}");
    }

    // Check if graph mode
    let is_graph = blueprint.stages.iter().any(|s| s.transitions.is_some());
    if is_graph {
        let entry = blueprint.resolve_entry_stage_name();
        println!("  Graph mode: entry stage '{}'", entry);

        // List stages and their transitions
        for stage in &blueprint.stages {
            let transitions_info = match &stage.transitions {
                Some(t) if !t.is_empty() => {
                    let targets: Vec<&str> = t.keys().map(|k| k.as_str()).collect();
                    format!(" → {}", targets.join(", "))
                }
                Some(_) => " (terminal)".to_string(),
                None => " (linear)".to_string(),
            };
            let revisits = stage
                .max_revisits
                .map(|n| format!(" (max_revisits: {})", n))
                .unwrap_or_default();
            println!("  - {}{}{}", stage.name, transitions_info, revisits);
        }
    } else {
        println!(
            "  Linear mode: {}",
            blueprint
                .stages
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(" → ")
        );
    }
}

/// Outcome of the real, testable logic in [`execute`]. Kept distinct from
/// the actual failure reporting so `execute_reporting_outcome` - and therefore
/// every branch of `check_manifest`'s error handling - can be unit tested.
#[derive(Debug)]
enum ValidateOutcome {
    Success,
    ParseError(String),
    ValidationError(String),
    /// The manifest is structurally fine but the lint found something fatal:
    /// how many errors, and how many warnings (which only count when
    /// `--deny-warnings` was passed).
    LintFailed {
        errors: usize,
        warnings: usize,
    },
}

/// Print `findings` worst-first, one per line with its fix indented under it.
///
/// Returns the counts so the caller can decide the exit status without walking
/// the list again.
fn print_findings(findings: &[LintFinding]) -> (usize, usize) {
    let mut errors = 0;
    let mut warnings = 0;
    for finding in findings {
        match finding.severity {
            LintSeverity::Error => errors += 1,
            LintSeverity::Warning => warnings += 1,
            LintSeverity::Note => {}
        }
        println!(
            "  {} {} [{}]",
            finding.severity.label(),
            finding.one_line(),
            finding.code
        );
        if let Some(fix) = &finding.fix {
            println!("       {fix}");
        }
    }
    (errors, warnings)
}

/// The command core. `config` is the user's configuration when it could be
/// loaded, and is only used to answer "can this install reach the providers
/// this blueprint names" - a config that will not load is not a reason to
/// refuse to lint, it only means that one check has nothing to say. Taking it
/// as an argument keeps this function hermetic; the real
/// [`Config::load`](crate::config::Config::load) happens in [`execute`].
fn execute_reporting_outcome(
    args: &ValidateArgs,
    config: Option<&crate::config::Config>,
    registry: Option<&leviath_runtime::ProviderRegistry>,
) -> anyhow::Result<ValidateOutcome> {
    let path = PathBuf::from(&args.path);

    let checked = match check_manifest(&path) {
        Ok(c) => c,
        Err(ManifestCheckError::Io(e)) => return Err(e),
        Err(ManifestCheckError::Parse(e)) => {
            if args.json {
                ValidateReport::failed(format!("parse error: {e}")).print();
            }
            return Ok(ValidateOutcome::ParseError(e));
        }
        Err(ManifestCheckError::Validation(e)) => {
            if args.json {
                ValidateReport::failed(format!("validation failed: {e}")).print();
            }
            return Ok(ValidateOutcome::ValidationError(e));
        }
    };

    // The human report is three separate printers. JSON is one document, so it
    // is built after the lint and emitted once, and none of these run.
    if !args.json {
        print_success(&checked.blueprint);
        print_script_tool_report(&path);
        if args.graph {
            println!();
            println!("{}", graph_text(&checked.blueprint, args.width));
        }
    }

    let mut env = LintEnv::offline(&checked.agent_dir);
    if let Some(config) = config {
        // The directory the command was run from is the workdir a `lev run`
        // would default to, so it is what relative `[read_paths]` entries
        // resolve against.
        let workdir = crate::commands::resolve_cwd().unwrap_or_default();
        if !args.json {
            print_model_resolution(&checked.blueprint, config, registry);
        }
        env = env
            .with_providers(&checked.blueprint, config)
            .with_read_paths(&checked.blueprint, config, &workdir);
    }
    let findings = lint_manifest(&checked.content, &checked.blueprint, &env);
    let (errors, warnings) = match args.json {
        true => {
            let report = ValidateReport::linted(&checked.blueprint, findings, args.deny_warnings);
            report.print();
            (report.errors, report.warnings)
        }
        false => print_findings(&findings),
    };

    if errors > 0 || (args.deny_warnings && warnings > 0) {
        return Ok(ValidateOutcome::LintFailed { errors, warnings });
    }
    Ok(ValidateOutcome::Success)
}

/// What each stage would actually dispatch to on this machine, and why.
///
/// A blueprint lists an ordered set of models per stage, and the resolver
/// reorders it: registered candidates on `default_provider` move to the front,
/// `default_model` first among them. Nothing surfaced the result, so a config
/// line could silently move every stage onto a fallback model and the only
/// evidence was in a finished run's metadata. The line under each stage is the
/// blueprint's own order, so the promotion is visible as a difference rather
/// than something to take on trust.
fn print_model_resolution(
    blueprint: &leviath_core::Blueprint,
    config: &crate::config::Config,
    registry: Option<&leviath_runtime::ProviderRegistry>,
) {
    // A registry that will not build says nothing here rather than failing the
    // validation: this block is extra information about an install, and the
    // lint below has its own thing to say about unreachable providers.
    let Some(registry) = registry else {
        return;
    };
    for line in model_resolution_lines(blueprint, config, registry) {
        println!("{line}");
    }
}

/// Whether anything registered here can run this entry.
///
/// A pinned entry needs its provider registered. An open one needs some provider
/// to claim the model, which is the same question the resolver asks.
fn model_is_reachable(
    entry: &leviath_core::blueprint::ModelEntry,
    registry: &leviath_runtime::ProviderRegistry,
) -> bool {
    if !entry.provider.is_empty() {
        return registry.has(&entry.provider);
    }
    let key = model_key(&entry.model);
    registry
        .native_providers()
        .iter()
        .any(|(_, p)| p.serves_model(key).is_some())
}

/// A model id without its vendor prefix, for comparing what was asked for
/// against what resolved.
///
/// The same model is spelled differently by route (`gpt-5.5` on OpenAI,
/// `openai/gpt-5.5` on a gateway), so comparing the full ids would report a
/// substitution every time a gateway serves the model the blueprint named.
fn model_key(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// The lines [`print_model_resolution`] prints, so they can be asserted
/// without capturing stdout.
fn model_resolution_lines(
    blueprint: &leviath_core::Blueprint,
    config: &crate::config::Config,
    registry: &leviath_runtime::ProviderRegistry,
) -> Vec<String> {
    let defaults = crate::daemon::spawn::model_defaults(config);
    let mut lines = vec![String::new(), "Models this install would use:".to_string()];
    for stage in &blueprint.stages {
        // `resolve_stage_model` rather than the candidate list: it carries the
        // "always at least one entry" invariant, so there is no empty case to
        // write a branch for and then never reach.
        let (provider, model) =
            leviath_runtime::pipeline::resolve_stage_model(&stage.model, None, &defaults, registry);
        let head = format!("{provider}/{model}");
        lines.push(format!("  {:<16} {head}", stage.name));

        // The blueprint's own first choice, and whether this machine can run
        // it. An entry nothing serves is skipped silently at resolution, so the
        // stage works on something further down and the only evidence is that
        // the top one is not what ran - which is a typo, a renamed model and an
        // unprimed gateway all wearing the same face.
        //
        // Only the FIRST entry earns a warning. A later one that cannot run here
        // is usually the machine declining an option rather than a fault: every
        // bundled blueprint ends with Ollama so a machine running one can use it,
        // and listing that as unserved on a machine that is not would read as a
        // fault list and bury the one line that matters.
        let reachable = stage
            .model
            .models
            .iter()
            .filter(|e| model_is_reachable(e, registry))
            .count();
        if let Some(first) = stage.model.models.first()
            && !model_is_reachable(first, registry)
        {
            lines.push(format!(
                "  {:<16}   prefers {}, which no configured provider serves - running {model}",
                "", first.model
            ));
        }
        // Nothing behind whatever is running: a provider outage mid-run ends the
        // stage rather than moving it along.
        if reachable == 1 {
            lines.push(format!(
                "  {:<16}   no fallback here: {model} is the only one this install can run",
                ""
            ));
        }
        // Written the way the blueprint writes them: a bare name for an entry
        // that left the route open, `provider/model` for one that pinned it.
        // Rendering an open entry as `/gpt-5.5` would show a route it does not
        // claim to have.
        let listed: Vec<String> = stage
            .model
            .models
            .iter()
            .map(|e| {
                if e.provider.is_empty() {
                    e.model.clone()
                } else {
                    format!("{}/{}", e.provider, e.model)
                }
            })
            .collect();
        // Only when the install disagrees with the blueprint: printing the
        // list under every stage that already got its first choice is noise,
        // and the point of the line is to make a substitution visible.
        //
        // Compared by MODEL, not by the whole route. An open entry names no
        // provider, so comparing the rendered strings would differ every time
        // and print the list under every stage.
        let first_model = stage.model.models.first().map(|e| e.model.as_str());
        if first_model.is_some_and(|first| model_key(first) != model_key(&model)) {
            lines.push(format!(
                "  {:<16}   blueprint order: {}",
                "",
                listed.join(", ")
            ));
        }
    }
    if !config.default_provider.is_empty() {
        let model = config.default_model.as_deref().unwrap_or("(unset)");
        lines.push(format!(
            "  default_provider = {}, default_model = {model}",
            config.default_provider
        ));
    }
    lines
}

/// The stage graph as text, the way the dashboard's stage explorer draws it
/// (escape edges included, since there is no key to reveal them here).
fn graph_text(blueprint: &leviath_core::Blueprint, width: u16) -> String {
    let graph = crate::tui::flowgraph::StageGraph::from_blueprint(blueprint);
    crate::tui::flowgraph::text::render_to_text(&graph, width)
}

/// The failure line for a lint that came back fatal. Split out so its
/// pluralization is assertable without capturing stdout.
fn lint_failure_message(errors: usize, warnings: usize, deny_warnings: bool) -> String {
    let mut parts = Vec::new();
    if errors > 0 {
        parts.push(format!("{errors} error{}", plural(errors)));
    }
    if deny_warnings && warnings > 0 {
        parts.push(format!(
            "{warnings} warning{} (--deny-warnings)",
            plural(warnings)
        ));
    }
    format!("✗ Blueprint has {}", parts.join(" and "))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Validate the agent's own Rhai script tools: discover the agent
/// directory's `tools/` and report how many compiled, warning (non-fatal, like
/// the daemon's own skip-and-warn) about any that failed. A missing `tools/` dir
/// prints nothing.
fn print_script_tool_report(path: &std::path::Path) {
    // The agent dir is the manifest's parent (file path) or the path itself (dir).
    let agent_dir = if path.is_file() {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    };
    let tools_dir = agent_dir.join("tools");
    if !tools_dir.is_dir() {
        return;
    }
    let (set, skipped) = leviath_scripting::ScriptToolSet::discover(&[tools_dir]);
    if !set.is_empty() {
        println!("  {} script tool(s) in tools/", set.len());
    }
    // A tool that compiles but whose `@requires` the platform can't satisfy won't
    // load - flag it (this also catches an unknown/typo'd capability name).
    for meta in set.metas() {
        if !crate::daemon::spawn::current_platform_satisfies(&meta.required_caps) {
            println!(
                "  ⚠ Warning: script tool '{}' won't load here (unsatisfiable @requires: {})",
                meta.name,
                meta.required_caps.join(", ")
            );
        }
    }
    for s in &skipped {
        println!(
            "  ⚠ Warning: script tool '{}' skipped: {}",
            s.path.display(),
            s.reason
        );
    }
}

/// The provider registry this install would use, with every provider's model
/// list already fetched.
///
/// `None` when there is no config to build one from, or when it will not build:
/// both mean this command has nothing to say about which model a stage runs, and
/// neither is a validation failure - the lints have their own thing to say about
/// an install with no reachable provider.
async fn primed_registry(
    config: Option<&crate::config::Config>,
) -> Option<leviath_runtime::ProviderRegistry> {
    primed_registry_with(config, &leviath_providers::provider::build_http_client).await
}

/// [`primed_registry`], with client construction injected.
///
/// The same seam [`build_provider_registry_from_config_with`] carries, and for
/// the same reason: reqwest will not fail to build a client in any environment a
/// test can arrange, so the "no registry" path is only reachable through here.
///
/// [`build_provider_registry_from_config_with`]:
///     crate::commands::run::build_provider_registry_from_config_with
async fn primed_registry_with(
    config: Option<&crate::config::Config>,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> Option<leviath_runtime::ProviderRegistry> {
    let config = config?;
    let registry =
        crate::commands::run::build_provider_registry_from_config_with(config, build_client)
            .ok()?;
    registry
        .prime_capabilities(
            std::time::Duration::from_secs(VALIDATE_PRIME_TIMEOUT_SECS),
            // So `lev validate` reports the model a run would really use on a
            // machine whose default is a script provider, rather than the one
            // it would have used before that provider could answer.
            Some(config.default_provider.as_str()),
        )
        .await;
    Some(registry)
}

/// How long `lev validate` waits for a provider's model list.
///
/// Shorter than the daemon's: the daemon is starting up once and every run after
/// it depends on the answer, while this is a command someone is waiting on. A
/// provider that does not answer in time falls back to the compiled-in table,
/// and `prime_capabilities` warns saying which one, so the difference is visible
/// rather than silently changing which model the output names.
const VALIDATE_PRIME_TIMEOUT_SECS: u64 = 5;

/// Run `lev validate`: check a blueprint and print what is wrong with it.
pub async fn execute(args: ValidateArgs) -> anyhow::Result<()> {
    let config = crate::config::Config::load().ok();
    // Appended to a load failure, and only when the file is an installed copy
    // of a bundled agent this build ships a different version of. Then the
    // answer is "reinstall it", not "debug your graph".
    let stale = || {
        crate::bundled::stale_install_suffix(
            &manifest_path_for(std::path::Path::new(&args.path)),
            crate::bundled::real_agents_dir_opt().as_deref(),
            "\n\n",
        )
    };
    // Primed exactly as the daemon primes, and for the same reason: a provider
    // whose model list is a network call away answers from the compiled-in
    // table until it is asked. Without this, `validate` and the daemon disagree
    // about which model a stage runs, and the tool whose job is saying what will
    // happen is the one that does not know.
    let registry = primed_registry(config.as_ref()).await;

    match execute_reporting_outcome(&args, config.as_ref(), registry.as_ref())? {
        ValidateOutcome::Success => Ok(()),
        ValidateOutcome::ParseError(e) => anyhow::bail!("✗ Parse error: {}{}", e, stale()),
        ValidateOutcome::ValidationError(e) => {
            anyhow::bail!("✗ Validation failed: {}{}", e, stale())
        }
        ValidateOutcome::LintFailed { errors, warnings } => {
            anyhow::bail!(lint_failure_message(errors, warnings, args.deny_warnings))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_test_agent;

    /// The line that would have answered "why is this run on deepseek".
    ///
    /// `default_provider` moves its registered candidates to the front of a
    /// stage's list, so an install can dispatch somewhere the blueprint did
    /// not ask for first. Nothing surfaced that, so the substitution is shown
    /// against the blueprint's own order - and only when they differ, because
    /// repeating the list under a stage that got its first choice is noise.
    /// A blueprint leading with a model nothing configured serves falls through
    /// to the next one, and the listing says so. This is the surviving reason
    /// for a substitution: #578 removed the other one, where preferring a
    /// provider silently changed which model ran.
    #[test]
    fn model_resolution_explains_falling_through_to_the_next_model() {
        let manifest = r#"
[agent]
name = "m"
version = "0.1.0"
entry_stage = "one"

[stages.one]
model = { models = ["a-model-nobody-serves", "claude-sonnet-5"] }
system_prompt = "hi"
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_agent(dir.path(), manifest);
        let checked = check_manifest(&path).expect("the manifest parses");
        let blueprint = &checked.blueprint;

        let config = crate::config::Config {
            default_provider: "anthropic".to_string(),
            default_model: None,
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("test-key".to_string()),
                ..Default::default()
            },
            ..crate::config::Config::default()
        };
        let registry = crate::commands::run::build_provider_registry_from_config_probing(
            &config,
            &leviath_providers::provider::build_http_client,
            &|_| false,
        )
        .expect("an HTTPS client builds in tests");

        let lines = model_resolution_lines(blueprint, &config, &registry);
        assert!(
            lines.iter().any(|l| l.contains("blueprint order")),
            "the substitution is explained: {lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("a-model-nobody-serves")),
            "and it names what was asked for: {lines:#?}"
        );
        // The open-route entries are printed as the blueprint wrote them, not
        // as `/claude-sonnet-5` with an empty provider in front.
        // Checked on the blueprint-order line alone: the resolved line above it
        // legitimately shows the route the run will take.
        let order = lines
            .iter()
            .find(|l| l.contains("blueprint order"))
            .expect("the line asserted above");
        assert!(
            !order.contains("/claude-sonnet-5"),
            "an entry that pinned no route is not shown with one: {order}"
        );
    }

    /// With no config there is no install to describe, so there is no registry
    /// and the models block says nothing. Not a validation failure: the
    /// blueprint is still checked, it is only the "what would run here" part
    /// that has no answer.
    #[tokio::test]
    async fn no_config_means_no_registry_to_prime() {
        assert!(
            primed_registry(None).await.is_none(),
            "nothing to build a registry from"
        );
    }

    /// A machine that cannot build an HTTPS client cannot reach any provider, so
    /// the same silence applies. Reached through the injected factory, because
    /// reqwest will not fail to build a client in any environment a test can
    /// arrange.
    #[tokio::test]
    async fn a_machine_with_no_https_client_has_no_registry_to_prime() {
        // Both providers are pinned, because a client is only ever built for a
        // provider that registers. A default config registers whichever ones the
        // machine happens to offer: the key comes from the environment, and
        // Ollama registers on answering at its default address. On a developer
        // machine running Ollama that is enough to reach the failure; on CI,
        // where neither is present, nothing registers, no client is asked for,
        // and an empty registry builds cleanly. Naming a key and an address that
        // resolves nowhere makes the test the same everywhere.
        let config = crate::config::Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test".to_string()),
                ..Default::default()
            },
            ollama_base_url: Some("http://127.0.0.1:1/".to_string()),
            ..Default::default()
        };
        // The purpose-built error rather than a TLS-backend trick: which
        // backend refuses a bogus configuration differs by platform, so that
        // version passed on macOS and failed on Linux.
        let no_client = |_: Option<u64>| Err(leviath_providers::provider::malformed_url_error());
        assert!(
            primed_registry_with(Some(&config), &no_client)
                .await
                .is_none(),
            "no client, so no providers, so nothing to say"
        );
    }

    #[test]
    fn model_resolution_shows_where_the_install_overrides_the_blueprint() {
        let manifest = r#"
[agent]
name = "m"
version = "0.1.0"
entry_stage = "one"

[stages.one]
model = { models = [
  { provider = "anthropic", model = "claude-sonnet-5" },
  { provider = "openrouter", model = "deepseek/deepseek-v4-flash" },
] }
system_prompt = "hi"
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_agent(dir.path(), manifest);
        let checked = check_manifest(&path).expect("the manifest parses");
        let blueprint = &checked.blueprint;

        // A key is all it takes to register, and registration is all the
        // resolver asks about - so the real providers stand in for themselves
        // rather than a fake that would need a whole trait impl to answer one
        // question. Ollama is probed away: whether this machine is running it
        // is not part of what the test is about.
        let with_keys = |default_provider: &str| crate::config::Config {
            default_provider: default_provider.to_string(),
            default_model: None,
            openrouter_api_key: Some("test-key".to_string()),
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("test-key".to_string()),
                ..Default::default()
            },
            ..crate::config::Config::default()
        };
        let registry_for = |config: &crate::config::Config| {
            crate::commands::run::build_provider_registry_from_config_probing(
                config,
                &leviath_providers::provider::build_http_client,
                &|_| false,
            )
            .expect("an HTTPS client builds in tests")
        };

        // Preferring anthropic: the blueprint already leads with it, so there
        // is nothing to report and no second line.
        let anthropic_first = with_keys("anthropic");
        let registry = registry_for(&anthropic_first);
        let lines = model_resolution_lines(blueprint, &anthropic_first, &registry);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("anthropic/claude-sonnet-5")),
            "{lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("blueprint order")),
            "no substitution, so nothing to explain: {lines:#?}"
        );

        // Preferring openrouter no longer substitutes a different MODEL. The
        // blueprint asked for claude-sonnet-5 first and a route to it exists,
        // so that is what runs: `default_provider` chooses between routes to a
        // model, not between models (#578). Before this, the second-listed
        // entry was promoted and the run silently went to deepseek.
        let openrouter_first = with_keys("openrouter");
        let registry = registry_for(&openrouter_first);
        let lines = model_resolution_lines(blueprint, &openrouter_first, &registry);
        assert!(
            lines.iter().any(|l| l.contains("claude-sonnet-5")),
            "the blueprint's first model still wins: {lines:#?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("openrouter/deepseek/deepseek-v4-flash")),
            "a preference for a provider must not pick a different model: {lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("default_provider = openrouter")
                    && l.contains("default_model = (unset)")),
            "and the setting responsible has to be named: {lines:#?}"
        );

        // No preference expressed: blueprint order stands on its own and
        // there is no setting to name, so the trailing line is omitted.
        let no_preference = crate::config::Config {
            default_provider: String::new(),
            ..with_keys("anthropic")
        };
        let registry = registry_for(&no_preference);
        let lines = model_resolution_lines(blueprint, &no_preference, &registry);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("anthropic/claude-sonnet-5")),
            "{lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("default_provider =")),
            "nothing to name: {lines:#?}"
        );
    }

    /// A minimal manifest that lints clean, so a test can add exactly the one
    /// defect it is about.
    ///
    /// Ollama is last in the models list because it registers with no
    /// credential: under the isolated config these tests run against, a
    /// blueprint naming only keyed providers would (correctly) warn that
    /// nothing in its list is reachable.
    const CLEAN_MANIFEST: &str = r#"
[agent]
name = "ok-agent"
version = "0.1.0"
description = "Valid"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }, { provider = "ollama", model = "qwen3.5:9b" }] }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
"#;

    fn write_manifest(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
        let path = dir.join("agent.leviath");
        std::fs::write(&path, content).unwrap();
        path
    }

    fn args_for(dir: &std::path::Path) -> ValidateArgs {
        ValidateArgs {
            path: dir.to_str().unwrap().to_string(),
            deny_warnings: false,
            json: false,
            graph: false,
            width: 120,
        }
    }

    #[test]
    fn graph_text_draws_every_stage_and_the_flag_prints_it() {
        let toml = make_blueprint_toml(
            r#"
[stages.plan]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
[stages.plan.transitions.implement]
[stages.implement]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
[stages.implement.transitions.plan]
condition = "error"
[stages.implement.transitions.done]
[stages.done]
mode = "output"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
[stages.done.transitions]
"#,
        );
        let text = graph_text(&parse(&toml), 200);
        for stage in ["plan", "implement", "done"] {
            assert!(text.contains(stage), "{stage}: {text}");
        }
        assert!(text.contains("[error]"), "escape edges are drawn: {text}");
        // Through the command: the flag prints after the report and the
        // outcome is unchanged.
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), CLEAN_MANIFEST);
        let args = ValidateArgs {
            graph: true,
            width: 80,
            ..args_for(dir.path())
        };
        assert!(
            execute_reporting_outcome(&args, None, None)
                .unwrap()
                .is_success()
        );
    }

    /// With a config in hand, the report also says which model each stage
    /// would actually go to. Driven through the command so the `--json` guard
    /// is exercised: the resolution block is prose, and a caller parsing JSON
    /// must not find it spliced into the document.
    #[test]
    fn a_config_adds_the_model_resolution_to_the_prose_report() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), CLEAN_MANIFEST);
        let config = crate::config::Config {
            default_provider: "anthropic".to_string(),
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("test-key".to_string()),
                ..Default::default()
            },
            ..crate::config::Config::default()
        };
        assert!(
            execute_reporting_outcome(&args_for(dir.path()), Some(&config), None)
                .unwrap()
                .is_success()
        );
        // The same run as JSON: the block is suppressed, and the outcome is
        // the same either way.
        assert!(
            execute_reporting_outcome(&json_args_for(dir.path()), Some(&config), None)
                .unwrap()
                .is_success()
        );
    }

    // ─── print_success ───────────────────────────────────────────────────

    fn parse(toml: &str) -> leviath_core::Blueprint {
        leviath_core::manifest::parse_manifest(toml).unwrap()
    }

    /// Helper to create a minimal valid blueprint TOML with given stages.
    fn make_blueprint_toml(stages_toml: &str) -> String {
        format!(
            r#"
[agent]
name = "test"
version = "0.1.0"
description = "test blueprint"

{stages_toml}

[context.regions]
system = {{ kind = "pinned", max_tokens = 1000 }}
conversation = {{ kind = "sliding_window", max_items = 50, max_tokens = 10000 }}
"#
        )
    }

    #[test]
    fn print_success_linear_mode_no_panic() {
        let toml = make_blueprint_toml(
            r#"
[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main stage"
max_iterations = 5

[stages.review]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Review stage"
max_iterations = 5
"#,
        );
        print_success(&parse(&toml));
    }

    #[test]
    fn print_success_graph_mode_with_terminal_and_revisits_no_panic() {
        let toml = make_blueprint_toml(
            r#"
[stages.a]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "A"
max_iterations = 5
max_revisits = 3
[stages.a.transitions]
b = "true"

[stages.b]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "B"
max_iterations = 5
"#,
        );
        // Exercises: graph mode header, an edge with a target ("-> b"), and
        // stage "b" which has transitions = None ("(linear)" branch) as well
        // as the max_revisits formatting on stage "a".
        print_success(&parse(&toml));
    }

    #[test]
    fn print_success_graph_mode_terminal_stage_no_panic() {
        let toml = make_blueprint_toml(
            r#"
[stages.a]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "A"
max_iterations = 5
[stages.a.transitions]
b = "true"

[stages.b]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "B"
max_iterations = 5
[stages.b.transitions]
"#,
        );
        let bp = parse(&toml);
        // Stage "b" has an explicitly-empty transitions table -> Some(empty
        // map) -> exercises the "(terminal)" formatting branch.
        let b = bp.find_stage("b").unwrap();
        assert!(matches!(&b.transitions, Some(t) if t.is_empty()));
        print_success(&bp);
    }

    // ─── input_lines / input_summaries ───────────────────────────────────

    /// A reviewer-shaped manifest: no task region, one required input whose
    /// key differs from its region, one optional renamed input, and one bare
    /// optional input. Together the flags exercise every annotation
    /// combination the formatter has.
    const NAMED_INPUTS_MANIFEST: &str = r#"
[agent]
name = "inputs-agent"
version = "0.1.0"
description = "Named inputs"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-5" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
patch = { kind = "pinned", max_tokens = 2000, required = true, seed = "diff" }
review_criteria = { kind = "pinned", max_tokens = 1000, seed = "criteria" }
focus = { kind = "pinned", max_tokens = 500, seed = "input" }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
"#;

    /// The point of issue #414: validate now says what `lev run` would accept,
    /// including that `--task` is not among the flags.
    #[test]
    fn input_lines_name_every_flag_and_the_missing_task() {
        let lines = input_lines(&parse(NAMED_INPUTS_MANIFEST));
        assert_eq!(
            lines,
            vec![
                "  Inputs: --diff (required, seeds region 'patch'), \
                 --criteria (seeds region 'review_criteria'), --focus"
                    .to_string(),
                "  Note: this agent takes no --task; give it input via --diff, \
                 --criteria, --focus"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn input_lines_of_a_task_taking_agent_skip_the_refusal_note() {
        let toml = CLEAN_MANIFEST.replace(
            "[context.regions]",
            "[context.regions]\ntask = { kind = \"pinned\", max_tokens = 2000, \
             required = true, seed = \"task\" }",
        );
        let blueprint = parse(&toml);
        assert!(blueprint.accepts_task());
        assert_eq!(
            input_lines(&blueprint),
            vec!["  Inputs: --task (required)".to_string()],
            "an agent that takes a task needs no note about refusing one"
        );
    }

    #[test]
    fn input_lines_without_any_caller_input_say_so() {
        assert_eq!(
            input_lines(&parse(CLEAN_MANIFEST)),
            vec!["  Inputs: none - this agent takes no --task or other caller input".to_string()]
        );
    }

    /// The summaries feed the JSON report, so a harness can check an agent's
    /// inputs before spawning it instead of discovering the refusal at run
    /// time.
    #[test]
    fn input_summaries_carry_key_region_and_required() {
        let summaries = input_summaries(&parse(NAMED_INPUTS_MANIFEST));
        assert_eq!(
            summaries,
            vec![
                InputSummary {
                    key: "diff".to_string(),
                    region: "patch".to_string(),
                    required: true,
                },
                InputSummary {
                    key: "criteria".to_string(),
                    region: "review_criteria".to_string(),
                    required: false,
                },
                InputSummary {
                    key: "focus".to_string(),
                    region: "focus".to_string(),
                    required: false,
                },
            ]
        );
    }

    #[test]
    fn print_success_prints_the_input_lines_without_panicking() {
        // The formatting is asserted in the input_lines tests; this pins the
        // wiring, so the lines cannot silently drop out of the report.
        print_success(&parse(NAMED_INPUTS_MANIFEST));
    }

    // ─── print_findings ──────────────────────────────────────────────────

    /// One finding of each severity: the counts returned are errors and
    /// warnings only, because a note must never fail anything.
    #[test]
    fn print_findings_counts_errors_and_warnings_but_not_notes() {
        let findings = [
            (LintSeverity::Error, "e"),
            (LintSeverity::Error, "e2"),
            (LintSeverity::Warning, "w"),
            (LintSeverity::Note, "n"),
        ]
        .map(|(severity, code)| LintFinding {
            severity,
            code,
            stage: Some("main".to_string()),
            message: "something".to_string(),
            // Alternating so both the with-fix and without-fix print arms run.
            fix: (code == "e").then(|| "do the thing".to_string()),
        });
        assert_eq!(print_findings(&findings), (2, 1));
    }

    #[test]
    fn print_findings_on_an_empty_list_reports_nothing() {
        assert_eq!(print_findings(&[]), (0, 0));
    }

    // ─── lint_failure_message ────────────────────────────────────────────

    #[test]
    fn lint_failure_message_pluralizes_and_names_the_flag() {
        assert_eq!(lint_failure_message(1, 0, false), "✗ Blueprint has 1 error");
        assert_eq!(
            lint_failure_message(2, 5, false),
            "✗ Blueprint has 2 errors",
            "warnings are not counted unless they were asked to be"
        );
        assert_eq!(
            lint_failure_message(0, 1, true),
            "✗ Blueprint has 1 warning (--deny-warnings)"
        );
        assert_eq!(
            lint_failure_message(1, 2, true),
            "✗ Blueprint has 1 error and 2 warnings (--deny-warnings)"
        );
    }

    // ─── execute ─────────────────────────────────────────────────────────
    //
    // `execute` loads the real config, so each of these runs inside
    // `with_isolated_config_path_async`: it points the load at a scratch
    // directory and takes the same process-wide lock every other env-touching
    // test holds.

    #[tokio::test]
    async fn execute_parse_error_returns_error() {
        crate::config::with_isolated_config_path_async("validate-parse-error", |_| async {
            let dir = tempfile::tempdir().unwrap();
            write_manifest(dir.path(), "not valid toml [[[");
            let err = execute(args_for(dir.path())).await.unwrap_err();
            assert!(err.to_string().contains("Parse error"));
        })
        .await;
    }

    #[tokio::test]
    async fn execute_validation_error_returns_error() {
        crate::config::with_isolated_config_path_async("validate-validation-error", |_| async {
            let dir = tempfile::tempdir().unwrap();
            let manifest = r#"
[agent]
name = "bad-entry-agent"
version = "0.1.0"
description = "Entry stage does not exist"
entry_stage = "does-not-exist"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
            write_manifest(dir.path(), manifest);
            let err = execute(args_for(dir.path())).await.unwrap_err();
            assert!(err.to_string().contains("Validation failed"));
        })
        .await;
    }

    /// A tool name matching nothing is fatal, and the failure line says so.
    #[tokio::test]
    async fn execute_lint_error_fails_the_command() {
        crate::config::with_isolated_config_path_async("validate-lint-error", |_| async {
            let dir = tempfile::tempdir().unwrap();
            write_manifest(
                dir.path(),
                &CLEAN_MANIFEST.replace(
                    "max_iterations = 5",
                    "max_iterations = 5\navailable_tools = [\"raed_file\"]",
                ),
            );
            let err = execute(args_for(dir.path())).await.unwrap_err();
            assert_eq!(err.to_string(), "✗ Blueprint has 1 error");
        })
        .await;
    }

    /// A warning alone exits zero, and the same manifest fails under
    /// `--deny-warnings`. Asserted as a pair, since the whole point of the flag
    /// is the difference between the two.
    #[tokio::test]
    async fn warnings_only_fail_when_denied() {
        crate::config::with_isolated_config_path_async(
            "validate-deny-warnings",
            |cfg_dir| async move {
                // A key, so the blueprint's Anthropic entry is a reachable
                // provider. This used to ride on Ollama registering without a
                // credential; it now registers only when something answers at its
                // address, so on a machine with no local Ollama the manifest also
                // warned that nothing in its list was reachable - and the count
                // this test is about went from one to two.
                std::fs::write(
                    cfg_dir.join("config.toml"),
                    "[providers]\nanthropic_api_key = \"test-key\"\n",
                )
                .unwrap();
                let dir = tempfile::tempdir().unwrap();
                // No max_iterations on the one stage: exactly one warning, no errors.
                write_manifest(
                    dir.path(),
                    &CLEAN_MANIFEST.replace("max_iterations = 5", ""),
                );

                let mut args = args_for(dir.path());
                assert!(
                    execute_reporting_outcome(&args, None, None)
                        .unwrap()
                        .is_success()
                );

                args.deny_warnings = true;
                let err = execute(args).await.unwrap_err();
                assert_eq!(
                    err.to_string(),
                    "✗ Blueprint has 1 warning (--deny-warnings)"
                );
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_no_manifest_errors() {
        crate::config::with_isolated_config_path_async("validate-no-manifest", |_| async {
            let dir = tempfile::tempdir().unwrap();
            assert!(execute(args_for(dir.path())).await.is_err());
        })
        .await;
    }

    /// The manifest may be named directly rather than by its directory.
    #[tokio::test]
    async fn execute_valid_manifest_file_path() {
        crate::config::with_isolated_config_path_async("validate-file-path", |_| async {
            let dir = tempfile::tempdir().unwrap();
            let manifest_path = write_manifest(dir.path(), CLEAN_MANIFEST);
            let args = ValidateArgs {
                path: manifest_path.to_str().unwrap().to_string(),
                deny_warnings: false,
                json: false,
                graph: false,
                width: 120,
            };
            assert!(execute(args).await.is_ok());
        })
        .await;
    }

    #[tokio::test]
    async fn execute_valid_manifest_directory_path() {
        crate::config::with_isolated_config_path_async("validate-dir-path", |_| async {
            let dir = tempfile::tempdir().unwrap();
            write_test_agent(dir.path(), CLEAN_MANIFEST);
            assert!(execute(args_for(dir.path())).await.is_ok());
        })
        .await;
    }

    // ─── execute_reporting_outcome ───────────────────────────────────────

    impl ValidateOutcome {
        /// Whether this is [`ValidateOutcome::Success`]. A method rather than a
        /// `matches!` in each test: the never-taken arm of an inline `matches!`
        /// reads to llvm-cov as an uncovered region.
        fn is_success(&self) -> bool {
            matches!(self, Self::Success)
        }

        fn is_parse_error(&self) -> bool {
            matches!(self, Self::ParseError(_))
        }

        fn is_validation_error(&self) -> bool {
            matches!(self, Self::ValidationError(_))
        }
    }

    #[test]
    fn outcome_predicates_distinguish_the_variants() {
        assert!(ValidateOutcome::Success.is_success());
        assert!(!ValidateOutcome::Success.is_parse_error());
        assert!(!ValidateOutcome::Success.is_validation_error());
        assert!(ValidateOutcome::ParseError(String::new()).is_parse_error());
        assert!(ValidateOutcome::ValidationError(String::new()).is_validation_error());
        assert!(
            !ValidateOutcome::LintFailed {
                errors: 1,
                warnings: 0
            }
            .is_success()
        );
    }

    // ─── --json ──────────────────────────────────────────────────────────

    fn json_args_for(dir: &std::path::Path) -> ValidateArgs {
        ValidateArgs {
            json: true,
            ..args_for(dir)
        }
    }

    /// A finding of a given severity. `LintFinding::new` is private to `lint`,
    /// but the fields are public, so the report can be exercised from here
    /// without widening that API for a test.
    fn finding(severity: LintSeverity, code: &'static str) -> LintFinding {
        LintFinding {
            severity,
            code,
            stage: None,
            message: format!("{code} message"),
            fix: None,
        }
    }

    #[test]
    fn json_report_of_a_clean_manifest_is_valid_and_names_its_stages() {
        let blueprint = parse(CLEAN_MANIFEST);
        let report = ValidateReport::linted(&blueprint, Vec::new(), false);
        assert!(report.valid);
        assert_eq!(report.error, None);
        let summary = report.blueprint.expect("a parsed manifest has a summary");
        assert_eq!(summary.name, "ok-agent");
        assert_eq!(summary.stages, vec!["main".to_string()]);
        assert!(!summary.accepts_task);
        assert_eq!(summary.inputs, Vec::new());
        assert_eq!((report.errors, report.warnings, report.notes), (0, 0, 0));
    }

    #[test]
    fn json_report_counts_each_severity_separately() {
        let blueprint = parse(CLEAN_MANIFEST);
        let findings = vec![
            finding(LintSeverity::Error, "a"),
            finding(LintSeverity::Warning, "b"),
            finding(LintSeverity::Note, "c"),
        ];
        let report = ValidateReport::linted(&blueprint, findings, false);
        assert_eq!((report.errors, report.warnings, report.notes), (1, 1, 1));
        // An error is fatal whatever --deny-warnings says.
        assert!(!report.valid);
    }

    #[test]
    fn json_report_is_valid_with_a_warning_until_deny_warnings() {
        let blueprint = parse(CLEAN_MANIFEST);
        let warning = || vec![finding(LintSeverity::Warning, "b")];
        assert!(ValidateReport::linted(&blueprint, warning(), false).valid);
        assert!(!ValidateReport::linted(&blueprint, warning(), true).valid);
    }

    #[test]
    fn json_report_of_a_note_stays_valid_under_deny_warnings() {
        // Notes never fail a build. This is the rule most likely to drift, since
        // the JSON `valid` flag restates it in a second place.
        let blueprint = parse(CLEAN_MANIFEST);
        let notes = vec![finding(LintSeverity::Note, "c")];
        assert!(ValidateReport::linted(&blueprint, notes, true).valid);
    }

    #[test]
    fn json_report_of_a_broken_manifest_carries_the_error_and_no_blueprint() {
        let report = ValidateReport::failed("parse error: boom".to_string());
        assert!(!report.valid);
        assert!(report.blueprint.is_none());
        assert_eq!(report.error.as_deref(), Some("parse error: boom"));
    }

    #[test]
    fn json_report_serializes_every_key_a_caller_reads() {
        let blueprint = parse(CLEAN_MANIFEST);
        let report = ValidateReport::linted(
            &blueprint,
            vec![finding(LintSeverity::Error, "unknown-tool")],
            false,
        );
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(value["valid"], serde_json::json!(false));
        assert_eq!(value["blueprint"]["name"], serde_json::json!("ok-agent"));
        assert_eq!(value["error"], serde_json::Value::Null);
        // `code` is the stable slug a caller branches on, and `severity` is
        // lowercase rather than the padded table label.
        assert_eq!(
            value["findings"][0]["code"],
            serde_json::json!("unknown-tool")
        );
        assert_eq!(value["findings"][0]["severity"], serde_json::json!("error"));
    }

    /// The JSON half of issue #414: a harness reads the accepted inputs off
    /// the report instead of parsing the run-time refusal.
    #[test]
    fn json_report_names_the_accepted_inputs() {
        let blueprint = parse(NAMED_INPUTS_MANIFEST);
        let report = ValidateReport::linted(&blueprint, Vec::new(), false);
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(value["blueprint"]["accepts_task"], serde_json::json!(false));
        assert_eq!(
            value["blueprint"]["inputs"][0],
            serde_json::json!({"key": "diff", "region": "patch", "required": true})
        );
        assert_eq!(
            value["blueprint"]["inputs"][1]["key"],
            serde_json::json!("criteria")
        );
    }

    #[test]
    fn json_mode_still_reports_a_parse_error_through_the_outcome() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "not valid toml [[[");
        assert!(
            execute_reporting_outcome(&json_args_for(dir.path()), None, None)
                .unwrap()
                .is_parse_error()
        );
    }

    #[test]
    fn json_mode_still_reports_a_validation_error_through_the_outcome() {
        // A manifest that parses but names an entry stage that does not exist:
        // the other half of the failure path, and a different report line.
        let dir = tempfile::tempdir().unwrap();
        write_manifest(
            dir.path(),
            r#"
[agent]
name = "bad-entry-agent"
version = "0.1.0"
description = "Entry stage does not exist"
entry_stage = "does-not-exist"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#,
        );
        assert!(
            execute_reporting_outcome(&json_args_for(dir.path()), None, None)
                .unwrap()
                .is_validation_error()
        );
    }

    #[test]
    fn json_mode_still_succeeds_on_a_clean_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), CLEAN_MANIFEST);
        assert!(
            execute_reporting_outcome(&json_args_for(dir.path()), None, None)
                .unwrap()
                .is_success()
        );
    }

    #[test]
    fn execute_reporting_outcome_malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "not valid toml [[[");
        assert!(
            execute_reporting_outcome(&args_for(dir.path()), None, None)
                .unwrap()
                .is_parse_error()
        );
    }

    #[test]
    fn execute_reporting_outcome_bad_entry_stage_is_validation_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
[agent]
name = "bad-entry-agent"
version = "0.1.0"
description = "Entry stage does not exist"
entry_stage = "does-not-exist"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        write_manifest(dir.path(), manifest);
        assert!(
            execute_reporting_outcome(&args_for(dir.path()), None, None)
                .unwrap()
                .is_validation_error()
        );
    }

    #[test]
    fn execute_reporting_outcome_missing_manifest_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(execute_reporting_outcome(&args_for(dir.path()), None, None).is_err());
    }

    #[test]
    fn execute_reporting_outcome_valid_manifest_is_success() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), CLEAN_MANIFEST);
        assert!(
            execute_reporting_outcome(&args_for(dir.path()), None, None)
                .unwrap()
                .is_success()
        );
    }

    /// A blueprint whose regions run shell commands at spawn: the note lands in
    /// the findings, and does not fail the command.
    #[test]
    fn command_seed_regions_are_noted_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
[agent]
name = "scanner"
version = "0.1.0"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-5" }
description = "Main stage"
max_iterations = 5

[context.regions]
facts = { kind = "pinned", max_tokens = 1000, seed = { command = "git ls-files" } }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
"#;
        write_manifest(dir.path(), manifest);
        // Even under --deny-warnings, a note is not a warning.
        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
            deny_warnings: true,
            json: false,
            graph: false,
            width: 120,
        };
        assert!(
            execute_reporting_outcome(&args, None, None)
                .unwrap()
                .is_success()
        );
    }

    #[test]
    fn execute_reporting_outcome_reports_agent_script_tools() {
        // A valid agent whose `tools/` dir holds one good and one broken script:
        // validation still succeeds, and the script report's count + warning
        // branches both run.
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), CLEAN_MANIFEST);
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::write(tools.join("ok.rhai"), "// @tool ok\nparams.x").unwrap();
        std::fs::write(tools.join("bad.rhai"), "no directive\nlet").unwrap();
        // Compiles but requires an unsatisfiable capability → the won't-load warning.
        std::fs::write(tools.join("gpu.rhai"), "// @tool gpu\n// @requires gpu\n1").unwrap();
        assert!(
            execute_reporting_outcome(&args_for(dir.path()), None, None)
                .unwrap()
                .is_success()
        );
    }

    /// A tool the agent defines itself resolves, so granting it is not an
    /// unknown-tool error. This is the reason the lint env is built from the
    /// agent's own directory rather than from the built-ins alone.
    #[test]
    fn an_agents_own_script_tool_resolves() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(
            dir.path(),
            &CLEAN_MANIFEST.replace(
                "max_iterations = 5",
                "max_iterations = 5\navailable_tools = [\"stub_search\"]",
            ),
        );
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::write(
            tools.join("stub_search.rhai"),
            "// @tool stub_search\n// @description searches\n\"found\"",
        )
        .unwrap();
        assert!(
            execute_reporting_outcome(&args_for(dir.path()), None, None)
                .unwrap()
                .is_success()
        );
    }

    #[test]
    fn print_script_tool_report_no_tools_dir_is_silent() {
        // No `tools/` dir → the early return (covered by most success tests, but
        // asserted here directly against a file path, which exercises the
        // `path.is_file()` → parent arm).
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_manifest(dir.path(), "unused");
        print_script_tool_report(&manifest);
    }

    #[test]
    fn print_script_tool_report_only_broken_scripts_warns_without_count() {
        // A `tools/` dir with only a broken script: `set` is empty (no count
        // line - the `!set.is_empty()` false arm) but the skipped warning runs.
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::write(tools.join("bad.rhai"), "no directive\nlet").unwrap();
        print_script_tool_report(dir.path());
    }

    // ─── check_manifest ──────────────────────────────────────────────────

    #[test]
    fn check_manifest_verifies_custom_region_scripts() {
        // A custom region's script must exist and compile; the same failure a
        // spawn would hit, surfaced by `lev validate`.
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"
[agent]
name = "custom-validate"
version = "0.1.0"
description = "d"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-5" }
description = "Main stage"

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
brain = { kind = "custom", script = "hooks/brain.rhai", max_tokens = 1000 }
"#;
        let manifest_path = write_manifest(dir.path(), toml);

        // Missing script file → validation error naming region + path.
        let err = format!("{:?}", check_manifest(&manifest_path).unwrap_err());
        assert!(err.starts_with("Validation"), "{err}");
        assert!(err.contains("region 'brain'"), "{err}");

        // Present + compilable → passes.
        std::fs::create_dir(dir.path().join("hooks")).unwrap();
        std::fs::write(
            dir.path().join("hooks/brain.rhai"),
            "fn render(ctx) { \"ok\" }",
        )
        .unwrap();
        let checked = check_manifest(&manifest_path).unwrap();
        assert_eq!(checked.blueprint.name, "custom-validate");
        // The text is carried through for the linter, and the agent dir points
        // at the manifest's own directory rather than the manifest file.
        assert!(checked.content.contains("custom-validate"));
        assert_eq!(checked.agent_dir, dir.path());
    }

    /// Extract the inner `anyhow::Error` from a `ManifestCheckError::Io`,
    /// panicking with a diagnostic message for any other variant.
    fn unwrap_io_err(err: ManifestCheckError) -> anyhow::Error {
        let ManifestCheckError::Io(e) = err else {
            panic!("expected ManifestCheckError::Io, got {err:?}");
        };
        e
    }

    #[test]
    #[should_panic(expected = "expected ManifestCheckError::Io")]
    fn unwrap_io_err_panics_on_parse_variant() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "not valid toml [[[");
        let err = check_manifest(dir.path()).unwrap_err();
        // err is ManifestCheckError::Parse - this should panic
        unwrap_io_err(err);
    }

    #[test]
    fn check_manifest_missing_directory_manifest_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_manifest(dir.path()).unwrap_err();
        let e = unwrap_io_err(err);
        assert!(e.to_string().contains("No agent.leviath found"));
    }

    #[test]
    fn check_manifest_unreadable_file_path_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        // Pass a path to a file that doesn't exist directly (is_file() is
        // false, and it's not a directory either) - falls through to the
        // "join agent.leviath" branch, which also won't exist.
        let missing = dir.path().join("nonexistent-subdir");
        let err = check_manifest(&missing).unwrap_err();
        unwrap_io_err(err);
    }

    // Distinct from the two "file doesn't exist" IO-error cases above: this
    // exercises `std::fs::read_to_string`'s own `Err` arm (a manifest file
    // that *is* found via `path.is_file()`/`.exists()`, but can't actually
    // be read), which no other test reaches.
    #[test]
    fn check_manifest_unreadable_file_is_io_error() {
        // `agent.leviath` exists but is a *directory*, so it's found via
        // `.exists()` yet `read_to_string` fails on every platform, exercising
        // the read_to_string map_err arm.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("agent.leviath")).unwrap();

        let err = check_manifest(dir.path()).unwrap_err();
        let e = unwrap_io_err(err);
        assert!(e.to_string().contains("Failed to read"));
    }

    impl ManifestCheckError {
        /// Whether this is a parse failure. A method rather than an inline
        /// `matches!` in the test: the arm the passing run does not take reads
        /// to llvm-cov as an uncovered region, and so does a `{err:?}` argument
        /// that only a failing assertion would format.
        fn is_parse(&self) -> bool {
            matches!(self, Self::Parse(_))
        }
    }

    #[test]
    fn check_manifest_malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "not valid toml [[[");
        assert!(check_manifest(dir.path()).unwrap_err().is_parse());
        // And the other arm: a missing manifest is an I/O failure, not a parse
        // one, so the predicate is deciding rather than always agreeing.
        let empty = tempfile::tempdir().unwrap();
        assert!(!check_manifest(empty.path()).unwrap_err().is_parse());
    }

    #[test]
    fn check_manifest_direct_file_path_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = write_manifest(dir.path(), CLEAN_MANIFEST);
        // Pass the *file* path directly, not the directory.
        let checked = check_manifest(&manifest_path).unwrap();
        assert_eq!(checked.blueprint.name, "ok-agent");
    }
}
