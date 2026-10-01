//! Client-side helpers for talking to the shared-world daemon: building a spawn
//! request from local inputs and exchanging it over the control socket. Shared by
//! `lev run` (and reusable by other clients). The socket-path resolution + connect
//! live in the binary; these cores are unit-testable against a fake socket server.

use std::collections::HashMap;

use anyhow::bail;
use leviath_runtime::control_socket::{ControlClient, ControlResponse};
use leviath_runtime::spec::request::SpawnRequest;

use crate::commands::run::attach::{self, RegionInput};
use crate::commands::run::manifest::find_manifest;
use crate::commands::run::task::resolve_task;

/// Everything a spawn request needs from the agent's own files.
pub(crate) struct AgentSource {
    /// The resolved `agent.leviath` path.
    pub manifest: std::path::PathBuf,
    /// The parsed blueprint itself.
    pub blueprint: leviath_runtime::spec::Blueprint,
}

/// Find the agent's manifest and parse it, once.
///
/// The parse is unconditional rather than gated on there being region flags to
/// validate: the blueprint's name and description are needed for the editor
/// template too, and it costs nothing either way. This is the same parser the
/// daemon runs on the same file moments later, so a manifest that fails here
/// would have failed there, and `parse manifest: <toml error>` before the
/// daemon is contacted beats a spawn rejection after.
pub(crate) fn load_agent_source(path: &str) -> anyhow::Result<AgentSource> {
    let found = find_manifest(path)?;
    // Absolute, because this path is about to be handed to the daemon, which
    // has its own working directory. `lev run .` and `lev run ./demo` resolve
    // fine here and then arrive there as `./agent.leviath`, which the daemon
    // reads relative to wherever it happens to have been started - so the spawn
    // fails with "read manifest './agent.leviath': No such file or directory".
    // `lev create` prints `lev run .` as its next step, so that is the first
    // thing a new user hits.
    //
    // Best-effort rather than fallible: `find_manifest` only returns paths it
    // has already confirmed resolve, so a failure here needs the file to vanish
    // between the two calls. Falling back to what it found leaves a legible
    // daemon-side error rather than inventing an error arm no test can reach.
    let manifest = std::fs::canonicalize(&found).unwrap_or(found);
    let content = std::fs::read_to_string(&manifest)
        .map_err(|e| anyhow::anyhow!("read manifest '{}': {e}", manifest.display()))?;
    let blueprint = leviath_runtime::spec::manifest::parse_manifest(&content)
        .map_err(|e| anyhow::anyhow!("parse manifest: {e}"))?;
    Ok(AgentSource {
        manifest,
        blueprint,
    })
}

/// What the `--<region>` flags hold: each one's text, and the files its
/// `@path` tokens name, each bound for that region. A flag naming no region
/// the blueprint lets a caller fill is refused here, before anything else is
/// asked of the person, so a typo does not cost them a task typed into an
/// editor.
fn read_regions(
    blueprint: &leviath_runtime::spec::Blueprint,
    regions: HashMap<String, String>,
    cwd: &std::path::Path,
) -> anyhow::Result<RegionSeeds> {
    let declared = blueprint.caller_inputs();
    let registry = attach::cli_registry();
    let mut text = HashMap::new();
    let mut parts = Vec::new();
    let mut unresolved = Vec::new();
    for (name, raw) in regions {
        if !declared.contains(&name.as_str()) {
            let known = match declared.is_empty() {
                true => "(none)".to_string(),
                false => declared.join(", "),
            };
            anyhow::bail!(
                "unknown region '--{name}'; this agent's caller-input regions are: {known}"
            );
        }
        let RegionInput {
            text: said,
            parts: found,
            unresolved: missing,
        } = attach::read_region_input(&name, &raw, cwd, &registry)?;
        if !said.is_empty() {
            text.insert(name, said);
        }
        parts.extend(found);
        unresolved.extend(missing);
    }
    Ok(RegionSeeds {
        text,
        parts,
        unresolved,
    })
}

/// What the `--<region>` flags held.
struct RegionSeeds {
    /// Each region's text, by region.
    text: HashMap<String, String>,
    /// The files their `@path` tokens named, each bound for its region.
    parts: Vec<leviath_core::mime::InboundPart>,
    /// The `@path` tokens that named no file.
    unresolved: Vec<String>,
}

/// The stdin probe for a caller that must never open an editor for a task:
/// the dashboard, which owns the terminal itself. An editor launched under it
/// would fight it for the screen.
///
/// Passing this rather than a bare `|| false` states the reason at each call
/// site.
pub(crate) fn never_interactive() -> bool {
    false
}

/// What `lev run` was asked for, before any of it is resolved.
///
/// One struct because these are one thing: the command line. Each field is a
/// flag the user typed, and grouping them keeps the difference between "what was
/// asked for" and "what that resolves to" visible - [`run_request`] turns this
/// into a [`LocalRun`], and the two are deliberately different types.
pub struct RunLine<'a> {
    /// The blueprint path or name, as given.
    pub path: &'a str,
    /// The task text, if it was given rather than read from stdin or an editor.
    pub task: Option<&'a str>,
    /// Whether stdin is a terminal, injected so the editor path is testable.
    pub stdin_is_terminal: &'a dyn Fn() -> bool,
    /// `--model`, overriding the blueprint's choice.
    pub model: Option<String>,
    /// The working directory tools run in.
    pub workdir: &'a str,
    /// `--yolo`: run unattended.
    pub yolo: bool,
    /// `--yolo=<name>`: the profile that says which parts of unattended a
    /// person still wants. `None` is the bare flag.
    pub yolo_profile: Option<String>,
    /// `--allow`: tools permitted outright.
    pub allow: Vec<String>,
    /// `--max-depth`: sub-agent tree cap.
    pub max_depth: Option<usize>,
    /// `--<region>` text, keyed by the caller input it fills.
    pub regions: HashMap<String, String>,
    /// `--no-seed-commands`: refuse the blueprint's command seeds.
    pub no_seed_commands: bool,
    /// The output shape the caller asked for, overriding the blueprint's.
    pub output_request: Option<leviath_core::output::OutputSpec>,
    /// Files attached with `--attach`, already read: their paths were
    /// relative to where the command ran, which only the caller knows.
    pub parts: Vec<leviath_core::mime::InboundPart>,
}

/// A `lev run` command line, resolved locally: the request to send, and what
/// the warnings before it and the report after it read.
#[derive(Debug, Clone)]
pub struct LocalRun {
    /// The request.
    pub request: SpawnRequest,
    /// The manifest the request names.
    pub manifest: std::path::PathBuf,
    /// The working directory, as given.
    pub workdir: String,
    /// Whether the run is unattended.
    pub yolo: bool,
    /// The yolo profile, when one was named.
    pub yolo_profile: Option<String>,
    /// The output shape asked for, for the warning about retired checks.
    pub output: Option<leviath_core::output::OutputSpec>,
}

/// Resolve the local inputs of a `lev run`: find and parse the manifest, read
/// the `--<region>` flags, resolve the task, and build the request.
///
/// `task` is what `--task` was given, if anything. Left off, `resolve_task`
/// opens the user's editor, which is why `stdin_is_terminal` is threaded
/// through: the probe itself is real I/O and belongs to the binary, so callers
/// inject it (tests pass a `fn` that always says no). None of that happens for a
/// blueprint that takes no task: it is not asked for one, and giving it one is
/// an error rather than text with nowhere to go.
///
/// Regions are read *before* the task on purpose. A `--foo @missing` has to
/// fail before the user is dropped into an editor and types a paragraph they
/// are about to lose. Whether each region is one the blueprint takes is the
/// daemon's to say, with every other problem with the request.
pub fn run_request(line: RunLine<'_>) -> anyhow::Result<LocalRun> {
    let RunLine {
        path,
        task,
        stdin_is_terminal,
        model,
        workdir,
        yolo,
        yolo_profile,
        allow,
        max_depth,
        regions,
        no_seed_commands,
        output_request,
        parts: attached,
    } = line;
    let source = load_agent_source(path)?;
    let cwd = std::env::current_dir().unwrap_or_default();
    let RegionSeeds {
        text: regions,
        mut parts,
        mut unresolved,
    } = read_regions(&source.blueprint, regions, &cwd)?;
    parts.extend(attached);
    // An agent driven by named regions takes no task, so neither demanding one
    // nor opening an editor to write one would make sense - `lev run reviewer
    // --diff @x.patch` is a complete command line. Handing it one anyway is the
    // error, and it is the same message the daemon would give.
    //
    // The same command line stays complete when the blueprint *can* take a
    // task but does not insist on one: a caller who named a region or attached
    // a file has said what the run is for.
    let handed_in = !regions.is_empty() || !parts.is_empty();
    let task = match source.blueprint.accepts_task() {
        true if task.is_none() && handed_in && !source.blueprint.requires_task() => String::new(),
        true => resolve_task(
            task,
            &source.blueprint.name,
            &source.blueprint.description,
            stdin_is_terminal,
        )?,
        false => match task.map(str::trim).unwrap_or("") {
            "" => String::new(),
            _ => anyhow::bail!(source.blueprint.task_refusal()),
        },
    };
    // A file the task names where it mentions it: `edit @hero.png`.
    let (task, named, missing) = attach::inline_parts(&task, None, &cwd)?;
    parts.extend(named);
    unresolved.extend(missing);
    // The dashboard resolves a task's `@path` against its own workdir and hands
    // the part over, then the line above resolves the same token again against
    // the current directory - which, for a dashboard whose workdir is where it
    // was launched, is the same file. Drop the exact repeat (same region, name
    // and bytes), which no caller ever means.
    let mut deduped: Vec<leviath_core::mime::InboundPart> = Vec::with_capacity(parts.len());
    for part in parts {
        let dup = deduped
            .iter()
            .any(|k| k.region == part.region && k.name == part.name && k.data == part.data);
        if !dup {
            deduped.push(part);
        }
    }
    attach::warn_unresolved(&unresolved);
    let request = run_launch(LaunchFlags {
        manifest: &source.manifest,
        task,
        regions,
        parts: deduped,
        model,
        workdir,
        yolo,
        yolo_profile: yolo_profile.clone(),
        allow,
        max_depth,
        no_seed_commands,
        output: output_request.clone(),
    })?;
    Ok(LocalRun {
        request,
        manifest: source.manifest,
        workdir: workdir.to_string(),
        yolo,
        yolo_profile,
        output: output_request,
    })
}

/// `lev run`'s flags, resolved, for [`run_launch`].
struct LaunchFlags<'a> {
    manifest: &'a std::path::Path,
    task: String,
    regions: HashMap<String, String>,
    parts: Vec<leviath_core::mime::InboundPart>,
    model: Option<String>,
    workdir: &'a str,
    yolo: bool,
    yolo_profile: Option<String>,
    allow: Vec<String>,
    max_depth: Option<usize>,
    no_seed_commands: bool,
    output: Option<leviath_core::output::OutputSpec>,
}

/// The request `lev run`'s flags make: the blueprint the manifest belongs to,
/// the task and each `--<region>` as inputs, the files as attachments, and the
/// launch flags as the run's launch settings.
fn run_launch(flags: LaunchFlags<'_>) -> anyhow::Result<SpawnRequest> {
    let blueprint = flags
        .manifest
        .parent()
        .unwrap_or(flags.manifest)
        .to_string_lossy()
        .into_owned();
    crate::daemon::requests::TaskLaunch {
        blueprint,
        task: flags.task,
        regions: flags.regions,
        parts: flags.parts,
        model: flags.model,
        workdir: Some(flags.workdir.to_string()),
        unattended: flags.yolo,
        profile: flags.yolo_profile,
        allow: flags.allow,
        max_depth: flags.max_depth,
        no_seed_commands: flags.no_seed_commands,
        output: flags.output,
        ..Default::default()
    }
    .into_request()
    .map_err(anyhow::Error::msg)
}

/// Warn, on stderr, when the agent about to run declares `[read_paths]` the
/// active config does not grant.
///
/// The daemon already logs this at spawn, but into its own log, where the
/// person who just typed `lev run` never sees it - so the first sign of a
/// missing grant was a refused read partway through a run. Everything needed to
/// say it here is local: `lev run` resolves the manifest itself, and the config
/// is the same file the daemon reads.
///
/// Best-effort by design. An unreadable manifest or config is the daemon's to
/// report, and it will: this must never be the reason a run does not start.
fn warn_ungranted_read_paths(run: &LocalRun) {
    for line in read_path_warning_for_spawn(run) {
        eprintln!("{line}");
    }
}

/// The warning for a spawn request, read from the real manifest and config.
/// Empty when there is nothing to say, and empty when either file cannot be
/// read: see [`warn_ungranted_read_paths`] for why that is not an error here.
fn read_path_warning_for_spawn(run: &LocalRun) -> Vec<String> {
    let Some(blueprint) = crate::commands::run::manifest::blueprint_at(&run.manifest) else {
        return Vec::new();
    };
    let Ok(config) = crate::config::Config::load() else {
        return Vec::new();
    };
    spawn_warning_lines(&blueprint, &config, std::path::Path::new(&run.workdir))
}

/// The warning itself: one line saying what is refused, then the stanza that
/// would grant it. Pure, so the wording is testable without a daemon.
fn spawn_warning_lines(
    blueprint: &leviath_runtime::spec::Blueprint,
    config: &crate::config::Config,
    workdir: &std::path::Path,
) -> Vec<String> {
    let Some(Ok(report)) = crate::read_path_report::build(blueprint, config, workdir) else {
        return Vec::new();
    };
    let Some(warning) = report.warning_line() else {
        return Vec::new();
    };
    let mut lines = vec![warning];
    lines.push("  add to your config.toml:".to_string());
    lines.extend(
        report
            .grant_stanza()
            .into_iter()
            .map(|l| format!("    {l}")),
    );
    lines
}

/// Say, before the run starts, that the config file on disk does not load.
///
/// This is the one warning here that is not about the blueprint. The daemon
/// keeps serving the last config that loaded, so the run *works* - on settings
/// the user may have edited an hour ago and believes are in force. Every other
/// warning on this path exists because the daemon only said it in its own log;
/// this one existed nowhere at all.
fn warn_broken_config() {
    for line in broken_config_warning(&crate::config::Config::config_path()) {
        eprintln!("{line}");
    }
}

/// The warning for a config file at `path`. Empty when it loads, which is why
/// this is pure: the wording is worth a test and a real `~/.leviath` is not.
fn broken_config_warning(path: &std::path::Path) -> Vec<String> {
    let Some(fault) = crate::config::ConfigFault::check(path) else {
        return Vec::new();
    };
    vec![
        format!(
            "warning: '{}' does not load ({}); this run uses the last config that did",
            path.display(),
            fault.summary()
        ),
        "  fix the file and the next run picks it up; nothing needs restarting".to_string(),
    ]
}

/// Refuse `--yolo=<name>` for a profile the file does not have before the
/// daemon is asked, and say what the profile keeps for a person.
///
/// Not best-effort like the warnings beside it: the person named a specific
/// set of rules, and the daemon would refuse the same spawn a moment later
/// with the same words. Failing here saves the round trip and the placeholder
/// run directory. The bare flag and an attended run say nothing.
pub(crate) fn yolo_profile_preflight(
    yolo: bool,
    yolo_profile: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let profile = crate::yolo::resolve_for_spawn(yolo, yolo_profile)?;
    let Some(profile) = profile.filter(|p| !p.is_builtin_default()) else {
        return Ok(Vec::new());
    };
    let holds = profile.holds();
    if holds.is_empty() {
        return Ok(vec![format!(
            "--yolo={} allows everything the config does not deny; it keeps nothing for you",
            profile.name
        )]);
    }
    let mut lines = vec![format!("--yolo={} keeps these for you:", profile.name)];
    lines.extend(holds.into_iter().map(|line| format!("  {line}")));
    Ok(lines)
}

/// Say, before the run starts, that `--yolo` will still stop for a person.
///
/// `--yolo` means "run without me", so a run that stops anyway reads as a hang.
/// The daemon does lint the blueprint at spawn, but only into `daemon.log`,
/// which the person typing the command never sees.
///
/// Best-effort for the same reason as [`warn_ungranted_read_paths`]: an
/// unreadable manifest or config is the daemon's to report, and this must never
/// be why a run does not start.
fn warn_held_checkpoints(run: &LocalRun) {
    for line in held_checkpoint_warning_for_spawn(run) {
        eprintln!("{line}");
    }
}

/// The pre-flight block for a spawn request: the checkpoints a `--yolo` run
/// will still stop at, and whether the blueprint is behind the one this build
/// ships.
///
/// The staleness note is not gated on `--yolo`. An install that is versions
/// behind is worth saying however the run was launched, and it is the reason
/// this exists: nothing said it at the moment it mattered, so a run could keep
/// using an old blueprint long after the fix had shipped.
fn held_checkpoint_warning_for_spawn(run: &LocalRun) -> Vec<String> {
    let path = run.manifest.as_path();
    let Some(blueprint) = crate::commands::run::manifest::blueprint_at(path) else {
        return Vec::new();
    };
    let mut lines: Vec<String> =
        crate::bundled::stale_install_note(path, &blueprint, leviath_core::agents_dir().as_deref())
            .into_iter()
            .collect();
    if run.yolo {
        let timeout = crate::config::Config::load()
            .ok()
            .and_then(|c| c.limits.interaction_timeout_secs);
        lines.extend(crate::held_checkpoints::preflight_lines(
            &blueprint, timeout,
        ));
    }
    lines
}

/// Say, before the run starts, that `--output-format` retires the declared
/// shape checks.
///
/// Overriding the format retires any Rhai validator and JSON schema the
/// blueprint declared, because a check written for one shape cannot judge
/// another. That is deliberate and stays; what cannot stay is the silence. The
/// daemon logs the retirement at spawn, but into `daemon.log`, and the person
/// who typed the override is the one counting on a check that will not run.
fn warn_retired_output_checks(run: &LocalRun) {
    for line in retired_check_warning_for_spawn(run) {
        eprintln!("warning: {line}");
    }
}

/// The retirement warning for a spawn request, read from the real manifest.
/// Best-effort for the same reason as [`warn_ungranted_read_paths`].
fn retired_check_warning_for_spawn(run: &LocalRun) -> Vec<String> {
    crate::commands::run::manifest::retired_check_warnings_at(&run.manifest, run.output.as_ref())
}

/// What `lev run --json` prints on a successful spawn.
///
/// `lev run` hands the agent to the daemon and returns, so the run id is the
/// only handle a caller gets on the work it just started. Parsing it back out of
/// `spawned <id>` meant a caller had to match on prose; this is the same
/// information in a shape that does not change when the sentence does.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SpawnedRun {
    /// The run id to poll with `lev ps --json` and stop with `lev cancel`.
    pub run_id: String,
    /// The manifest the run was resolved from.
    pub blueprint_path: String,
    /// The directory the agent's file tools are confined to.
    pub workdir: String,
    /// Whether the run was started unattended.
    pub yolo: bool,
}

/// Render a spawn outcome for printing: JSON when `json`, else the sentence.
///
/// Split from [`send_spawn`] so both shapes are testable without a daemon.
pub(crate) fn spawn_report(spawned: &SpawnedRun, json: bool) -> String {
    match json {
        // Four owned scalars with no map keys to reject, so this cannot fail.
        true => serde_json::to_string_pretty(spawned).expect("a spawn report serializes"),
        false => format!("spawned {}", spawned.run_id),
    }
}

/// Render a batch spawn outcome: a JSON array when `json`, else one
/// `spawned <id>` sentence per line. The single-run report keeps its own
/// object/sentence shape via [`spawn_report`], so existing `--json` callers
/// parse exactly what they always did.
pub(crate) fn batch_report(spawned: &[SpawnedRun], json: bool) -> String {
    match json {
        true => serde_json::to_string_pretty(spawned).expect("spawn reports serialize"),
        false => spawned
            .iter()
            .map(|s| format!("spawned {}", s.run_id))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Send a resolved spawn request to the daemon and report the outcome, printing
/// the new run id on success.
///
/// Warnings go to stderr, so `--json` leaves stdout parseable on its own.
pub(crate) async fn send_spawn(
    client: &ControlClient,
    run: LocalRun,
    json: bool,
) -> anyhow::Result<()> {
    warn_before(&run)?;
    let spawned = spawn_once(client, &run).await?;
    println!("{}", spawn_report(&spawned, json));
    Ok(())
}

/// Every warning a run's blueprint and this machine call for, before the
/// daemon is asked. A yolo profile the file does not have is the one refusal.
fn warn_before(run: &LocalRun) -> anyhow::Result<()> {
    warn_broken_config();
    warn_ungranted_read_paths(run);
    for line in yolo_profile_preflight(run.yolo, run.yolo_profile.as_deref())? {
        eprintln!("{line}");
    }
    warn_held_checkpoints(run);
    warn_retired_output_checks(run);
    Ok(())
}

/// Send `count` copies of a resolved spawn request - the same agent, task, and
/// flags; the daemon gives each its own run id - and print one combined report.
///
/// This exists because spawn throughput from the CLI is otherwise bounded by
/// process startup: each `lev run` invocation pays binary launch plus a socket
/// round trip (~60 spawns/second in measurement), while the daemon itself
/// accepts spawns as fast as they arrive. One invocation carrying the whole
/// batch removes that bound without introducing any daemon-side cap.
///
/// `count == 1` defers to `send_spawn`, keeping today's single-run output
/// shapes. A mid-batch failure stops the batch and says how many runs had
/// already started - those runs keep running; `lev ps` lists them.
pub async fn send_spawn_batch(
    client: &ControlClient,
    run: LocalRun,
    count: usize,
    json: bool,
) -> anyhow::Result<()> {
    if count == 0 {
        bail!("--count must be at least 1");
    }
    if count == 1 {
        return send_spawn(client, run, json).await;
    }
    // The warnings describe the blueprint and the machine, not the individual
    // run: once.
    warn_before(&run)?;
    let mut spawned = Vec::with_capacity(count);
    for _ in 0..count {
        match spawn_once(client, &run).await {
            Ok(started) => spawned.push(started),
            Err(e) => bail!(
                "batch stopped after {} of {count} runs started (those keep \
                 running; see `lev ps`): {e}",
                spawned.len()
            ),
        }
    }
    println!("{}", batch_report(&spawned, json));
    Ok(())
}

/// One spawn exchange with the daemon, warnings and printing left to callers.
async fn spawn_once(client: &ControlClient, run: &LocalRun) -> anyhow::Result<SpawnedRun> {
    match client.spawn(run.request.clone()).await {
        Ok(ControlResponse::Spawned { run_id }) => Ok(SpawnedRun {
            run_id,
            blueprint_path: run.manifest.to_string_lossy().into_owned(),
            workdir: run.workdir.clone(),
            yolo: run.yolo,
        }),
        Ok(ControlResponse::Rejected { issues }) => bail!("spawn refused: {issues}"),
        Ok(ControlResponse::Error { message }) => bail!("spawn failed: {message}"),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
    }
}

#[cfg(test)]
impl Default for LocalRun {
    /// A run of a blueprint named `x`, attended, asking for nothing.
    fn default() -> Self {
        Self {
            request: SpawnRequest::new(leviath_runtime::spec::request::SpawnSource::Blueprint(
                leviath_runtime::spec::names::BlueprintRef::parse("x").expect("a name"),
            )),
            manifest: std::path::PathBuf::new(),
            workdir: String::new(),
            yolo: false,
            yolo_profile: None,
            output: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::control_socket::{ControlId, bind_control_listener, control_id};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::task::JoinHandle;

    /// The text of `run`'s `name` input, when it has one.
    fn input_of(run: &LocalRun, name: &str) -> Option<String> {
        match run.request.inputs.get(name) {
            Some(leviath_runtime::spec::inputs::RawInput::Text(t)) => Some(t.clone()),
            _ => None,
        }
    }

    /// `run`'s task, or nothing.
    fn task_of(run: &LocalRun) -> String {
        input_of(run, "task").unwrap_or_default()
    }

    /// The model `run` asked for, as written.
    fn model_of(run: &LocalRun) -> Option<String> {
        run.request.model.as_ref().map(ToString::to_string)
    }

    /// The directory of the blueprint `run` asked for.
    fn source_name(run: &LocalRun) -> String {
        serde_json::to_value(&run.request.source).unwrap()["blueprint_file"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn write_manifest(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::write(
            dir.join("agent.leviath"),
            crate::test_support::inline_coder_manifest(),
        )
        .unwrap();
        dir.join("agent.leviath")
    }

    #[test]
    fn run_request_finds_manifest_and_builds_request() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let manifest = write_manifest(&agent_dir);

        let args = run_request(RunLine {
            path: manifest.to_str().unwrap(),
            task: Some("do it"),
            stdin_is_terminal: &never_interactive,
            model: Some("m".to_string()),
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap();
        assert!(source_name(&args).ends_with("my-agent"));
        assert_eq!(task_of(&args), "do it");
        assert_eq!(model_of(&args).as_deref(), Some("m"));
        assert_eq!(
            args.manifest.to_string_lossy(),
            std::fs::canonicalize(&manifest).unwrap().to_string_lossy()
        );
        assert_eq!(args.workdir, "/work");
    }

    /// The daemon has its own working directory, so a relative `PATH` has to be
    /// resolved before the request leaves: `lev run .` reaching the daemon as
    /// `./agent.leviath` fails there, and it is the very command `lev create`
    /// prints as the next step.
    #[test]
    fn run_request_sends_an_absolute_blueprint_path_for_a_relative_input() {
        // Reading the CWD is enough to race the tests that *move* it: one of
        // them chdirs into a directory it then deletes, and a relative path
        // resolved against that instant cannot be found. Take the same lock
        // they do, so this only ever reads a CWD that is standing still.
        let _guard = crate::config::isolate_cwd_for_test();
        // Rooted in the current directory rather than the system temp dir, so
        // the relative path is trivially expressible. A temp dir is not
        // guaranteed to share a drive with the cwd, and on the Windows runner
        // it does not: the checkout is on D: and TEMP is on C:, between which
        // no relative path exists at all.
        let dir = tempfile::Builder::new()
            .prefix("lev-relpath-")
            .tempdir_in(".")
            .unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        write_manifest(&agent_dir);

        // `tempdir_in` hands back an absolute path even for a relative base, so
        // the relative form is rebuilt from its name.
        let relative = std::path::Path::new(".")
            .join(dir.path().file_name().unwrap())
            .join("my-agent");
        // A static message on purpose: a `relative.display()` in here is only
        // evaluated when the assertion fails, which leaves it as a permanently
        // uncovered region under the 100% gate.
        assert!(relative.is_relative(), "expected a relative path");

        let args = run_request(RunLine {
            path: relative.to_str().unwrap(),
            task: Some("do it"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap();
        assert!(args.manifest.is_absolute());
        assert!(args.manifest.ends_with("agent.leviath"));
    }

    #[test]
    fn run_request_errors_on_missing_manifest() {
        assert!(
            run_request(RunLine {
                path: "/no/such/agent",
                task: Some("t"),
                stdin_is_terminal: &never_interactive,
                model: None,
                workdir: "/work",
                yolo: false,
                yolo_profile: None,
                allow: Vec::new(),
                max_depth: None,
                regions: HashMap::new(),
                no_seed_commands: false,
                output_request: None,
                parts: Vec::new(),
            })
            .is_err()
        );
    }

    /// `--task <file>` end to end through the real wiring, not just through
    /// `resolve_task` in isolation.
    #[test]
    fn run_request_reads_the_task_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let manifest = write_manifest(&agent_dir);
        let task_file = dir.path().join("task.md");
        std::fs::write(&task_file, "  summarize the README  \n").unwrap();

        let args = run_request(RunLine {
            path: manifest.to_str().unwrap(),
            task: Some(task_file.to_str().unwrap()),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap();
        assert_eq!(task_of(&args), "summarize the README");
    }

    /// No `--task` and no terminal to open an editor on: the run is refused
    /// here, before the daemon is contacted.
    #[test]
    fn run_request_without_a_task_errors_when_stdin_is_not_a_tty() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let manifest = write_manifest(&agent_dir);

        let err = run_request(RunLine {
            path: manifest.to_str().unwrap(),
            task: None,
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("No task provided"), "got: {err}");
    }

    /// A blueprint driven by named regions, taking no task at all.
    fn write_taskless_manifest(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("agent.leviath"),
            r#"
[agent]
name = "diffonly"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
diff = { kind = "pinned", max_tokens = 4000, seed = "diff" }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#,
        )
        .unwrap();
        dir.join("agent.leviath")
    }

    /// A blueprint that takes a task without insisting on one, and a `diff`
    /// region: the bundled reviewer's shape, whose task region exists for the
    /// fan-out workers it spawns from itself.
    fn write_optional_task_manifest(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("agent.leviath"),
            r#"
[agent]
name = "diff-or-task"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
task = { kind = "pinned", max_tokens = 4000, seed = "task" }
diff = { kind = "pinned", max_tokens = 4000, seed = "diff" }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#,
        )
        .unwrap();
        dir.join("agent.leviath")
    }

    fn launch<'a>(
        manifest: &'a std::path::Path,
        task: Option<&'a str>,
        regions: HashMap<String, String>,
    ) -> RunLine<'a> {
        RunLine {
            path: manifest.to_str().unwrap(),
            task,
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        }
    }

    /// `--diff` with no `--task` on a blueprint whose task is optional spawns
    /// with an empty task; the same blueprint given nothing at all is still
    /// asked, and a task it is given still goes through.
    #[test]
    fn an_optional_task_is_not_demanded_when_a_region_was_handed_in() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_optional_task_manifest(&dir.path().join("diff-or-task"));
        let diff = HashMap::from([("diff".to_string(), "- a\n+ b".to_string())]);

        let args = run_request(launch(&manifest, None, diff.clone())).unwrap();
        assert_eq!(task_of(&args), "");
        assert_eq!(input_of(&args, "diff").as_deref(), Some("- a\n+ b"));

        let err = run_request(launch(&manifest, None, HashMap::new())).unwrap_err();
        assert!(err.to_string().contains("No task provided"), "got: {err}");

        let args = run_request(launch(&manifest, Some("look at b"), diff)).unwrap();
        assert_eq!(task_of(&args), "look at b");
    }

    /// A `required` task region is a demand, and a region on the side does not
    /// waive it: the run would only be refused at spawn with the same ask.
    #[test]
    fn a_required_task_is_still_demanded_beside_a_region() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_optional_task_manifest(&dir.path().join("insists"));
        let insisting = std::fs::read_to_string(&manifest)
            .unwrap()
            .replace("seed = \"task\" }", "seed = \"task\", required = true }");
        std::fs::write(&manifest, insisting).unwrap();
        let regions = HashMap::from([("diff".to_string(), "x".to_string())]);
        let err = run_request(launch(&manifest, None, regions)).unwrap_err();
        assert!(err.to_string().contains("No task provided"), "got: {err}");
    }

    /// `lev run diffonly --diff ...` is a complete command line, so no task is
    /// demanded and no editor is opened - which is the whole reason the demand
    /// is conditional rather than unconditional.
    #[test]
    fn an_agent_that_takes_no_task_is_not_asked_for_one() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_taskless_manifest(&dir.path().join("diffonly"));
        let mut regions = HashMap::new();
        regions.insert("diff".to_string(), "a patch".to_string());

        let args = run_request(RunLine {
            path: manifest.to_str().unwrap(),
            task: None,
            // Says stdin is not a TTY, so an unconditional demand would error
            // here rather than fall through to the editor.
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .expect("no task is required of an agent that takes none");
        assert_eq!(task_of(&args), "");
        assert_eq!(input_of(&args, "diff").as_deref(), Some("a patch"));
    }

    /// The other half: handing that agent a task is the error, and the message
    /// points at the input it does take.
    #[test]
    fn an_agent_that_takes_no_task_refuses_one() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_taskless_manifest(&dir.path().join("diffonly"));

        let err = run_request(RunLine {
            path: manifest.to_str().unwrap(),
            task: Some("review my code"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("declares no region to put it in"),
            "got: {msg}"
        );
        assert!(msg.contains("it takes: diff"), "got: {msg}");
    }

    /// A `--task` of nothing but whitespace is the same as none, so it must not
    /// trip the refusal.
    #[test]
    fn a_blank_task_is_not_a_task() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_taskless_manifest(&dir.path().join("diffonly"));

        let args = run_request(RunLine {
            path: manifest.to_str().unwrap(),
            task: Some("   "),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .expect("blank is the same as absent");
        assert_eq!(task_of(&args), "");
    }

    /// Pins the ordering: a typo'd region flag must fail *before* the user is
    /// dropped into an editor, or they type a paragraph and then lose it.
    #[test]
    fn run_request_rejects_a_bad_region_before_it_looks_at_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_region_manifest(&dir.path().join("reviewer"));
        let regions = HashMap::from([("bogus".to_string(), "x".to_string())]);

        let err = run_request(RunLine {
            path: manifest.to_str().unwrap(),
            task: None,
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("unknown region"), "got: {err}");
    }

    /// Write a manifest declaring a `criteria` caller-input region, returning its
    /// path.
    fn write_region_manifest(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("agent.leviath"),
            r#"
[agent]
name = "reviewer"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
task = { kind = "pinned", max_tokens = 4000, seed = "task_input" }
criteria = { kind = "pinned", max_tokens = 2000, seed = "input" }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#,
        )
        .unwrap();
        dir.join("agent.leviath")
    }

    #[test]
    fn run_request_resolves_declared_region_and_reads_at_path() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_region_manifest(&dir.path().join("reviewer"));
        let policy = dir.path().join("policy.md");
        std::fs::write(&policy, "  focus on safety  ").unwrap();

        let regions = HashMap::from([(
            "criteria".to_string(),
            format!("@{}", policy.to_string_lossy()),
        )]);
        let args = run_request(RunLine {
            path: manifest.to_str().unwrap(),
            task: Some("review it"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap();
        // `@path` was read and trimmed.
        assert_eq!(
            input_of(&args, "criteria").as_deref(),
            Some("focus on safety")
        );
    }

    #[test]
    fn run_request_unknown_region_reports_none_when_no_caller_inputs() {
        // A blueprint with zero caller-input regions: the error lists "(none)".
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("noinput");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("agent.leviath"),
            r#"
[agent]
name = "noinput"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
data = { kind = "pinned", max_tokens = 2000 }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#,
        )
        .unwrap();
        let manifest = agent_dir.join("agent.leviath");
        let regions = HashMap::from([("foo".to_string(), "x".to_string())]);
        let err = run_request(RunLine {
            path: manifest.to_str().unwrap(),
            task: Some("t"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("(none)"), "got: {err}");
    }

    #[test]
    fn run_request_manifest_read_error_surfaces() {
        // `find_manifest` accepts a dir whose `agent.leviath` merely *exists*; when
        // that entry is itself a directory, the client-side read fails (EISDIR).
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("dirmanifest");
        std::fs::create_dir_all(agent_dir.join("agent.leviath")).unwrap();
        let regions = HashMap::from([("x".to_string(), "y".to_string())]);
        let err = run_request(RunLine {
            path: agent_dir.to_str().unwrap(),
            task: Some("t"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("read manifest"), "got: {err}");
    }

    #[test]
    fn run_request_manifest_parse_error_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("badtoml");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("agent.leviath"),
            "this is : not = valid toml [[[",
        )
        .unwrap();
        let regions = HashMap::from([("x".to_string(), "y".to_string())]);
        let err = run_request(RunLine {
            path: agent_dir.join("agent.leviath").to_str().unwrap(),
            task: Some("t"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("parse manifest"), "got: {err}");
    }

    #[test]
    fn run_request_region_value_bad_file_errors() {
        // A declared region whose `@file` value can't be read → the error from
        // read_region_value propagates out of run_request.
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_region_manifest(&dir.path().join("reviewer"));
        let regions = HashMap::from([("criteria".to_string(), "@/no/such/file.md".to_string())]);
        let err = run_request(RunLine {
            path: manifest.to_str().unwrap(),
            task: Some("review it"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("Failed to read region file"),
            "got: {err}"
        );
    }

    #[test]
    fn run_request_rejects_unknown_region_flag() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_region_manifest(&dir.path().join("reviewer"));
        let regions = HashMap::from([("bogus".to_string(), "x".to_string())]);
        let err = run_request(RunLine {
            path: manifest.to_str().unwrap(),
            task: Some("review it"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown region '--bogus'"),
            "got: {err}"
        );
    }

    /// Bind a control listener at a fresh id under `dir` and serve one canned
    /// response, returning the id clients connect to and the server task.
    fn fake_daemon(
        dir: &std::path::Path,
        response_line: &'static str,
    ) -> (ControlId, JoinHandle<()>) {
        let id = control_id(dir);
        let mut listener = bind_control_listener(&id).unwrap();
        let handle = tokio::spawn(async move {
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _request = lines.next_line().await.unwrap();
            write_half
                .write_all(response_line.as_bytes())
                .await
                .unwrap();
            write_half.write_all(b"\n").await.unwrap();
        });
        (id, handle)
    }

    async fn send(response_line: &'static str) -> anyhow::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let (id, server) = fake_daemon(dir.path(), response_line);
        let result = send_spawn(&ControlClient::new(id), LocalRun::default(), false).await;
        server.await.unwrap();
        result
    }

    /// Like [`fake_daemon`], but serves one canned response per connection, in
    /// order - the shape a batch spawn produces, since the client dials the
    /// socket once per request.
    fn fake_daemon_serving(
        dir: &std::path::Path,
        responses: Vec<&'static str>,
    ) -> (ControlId, JoinHandle<()>) {
        let id = control_id(dir);
        let mut listener = bind_control_listener(&id).unwrap();
        let handle = tokio::spawn(async move {
            for response_line in responses {
                let stream = listener
                    .accept()
                    .await
                    .expect("accept succeeds")
                    .expect("our own connection is admitted");
                let (read_half, mut write_half) = tokio::io::split(stream);
                let mut lines = BufReader::new(read_half).lines();
                let _request = lines.next_line().await.unwrap();
                write_half
                    .write_all(response_line.as_bytes())
                    .await
                    .unwrap();
                write_half.write_all(b"\n").await.unwrap();
            }
        });
        (id, handle)
    }

    #[tokio::test]
    async fn a_batch_spawn_starts_count_runs_and_reports_them_all() {
        let dir = tempfile::tempdir().unwrap();
        let (id, server) = fake_daemon_serving(
            dir.path(),
            vec![
                r#"{"result":"spawned","run_id":"a-1-000000000001"}"#,
                r#"{"result":"spawned","run_id":"a-1-000000000002"}"#,
                r#"{"result":"spawned","run_id":"a-1-000000000003"}"#,
            ],
        );
        let args = LocalRun {
            ..LocalRun::default()
        };
        send_spawn_batch(&ControlClient::new(id), args, 3, false)
            .await
            .expect("all three spawn");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_batch_stopped_mid_way_says_how_many_runs_already_started() {
        let dir = tempfile::tempdir().unwrap();
        let (id, server) = fake_daemon_serving(
            dir.path(),
            vec![
                r#"{"result":"spawned","run_id":"a-1-000000000001"}"#,
                r#"{"result":"error","message":"the world is full"}"#,
            ],
        );
        let err = send_spawn_batch(&ControlClient::new(id), LocalRun::default(), 3, false)
            .await
            .expect_err("the second spawn fails");
        // Asserts before the server join: a wrong error path makes fewer
        // connections than the server expects, and joining first would turn
        // that mismatch into a hang instead of a failure message.
        let text = err.to_string();
        assert!(text.contains("after 1 of 3"), "got: {text}");
        assert!(text.contains("the world is full"), "got: {text}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_batch_of_one_is_exactly_a_single_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let (id, server) = fake_daemon(dir.path(), r#"{"result":"spawned","run_id":"solo-1-0"}"#);
        send_spawn_batch(&ControlClient::new(id), LocalRun::default(), 1, false)
            .await
            .expect("the single spawn succeeds");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_batch_of_zero_is_refused_before_any_daemon_contact() {
        let dir = tempfile::tempdir().unwrap();
        // No listener bound: reaching the daemon at all would error differently.
        let id = control_id(dir.path());
        let err = send_spawn_batch(&ControlClient::new(id), LocalRun::default(), 0, false)
            .await
            .expect_err("zero runs is a refusal");
        assert!(err.to_string().contains("at least 1"), "got: {err}");
    }

    #[test]
    fn a_batch_report_lists_one_sentence_per_run() {
        let runs = vec![
            SpawnedRun {
                run_id: "a-1-1".into(),
                blueprint_path: "/b".into(),
                workdir: "/w".into(),
                yolo: false,
            },
            SpawnedRun {
                run_id: "a-1-2".into(),
                blueprint_path: "/b".into(),
                workdir: "/w".into(),
                yolo: false,
            },
        ];
        assert_eq!(batch_report(&runs, false), "spawned a-1-1\nspawned a-1-2");
        let parsed: Vec<SpawnedRun> =
            serde_json::from_str(&batch_report(&runs, true)).expect("a JSON array");
        assert_eq!(parsed, runs);
    }

    fn spawned() -> SpawnedRun {
        SpawnedRun {
            run_id: "coder-1".to_string(),
            blueprint_path: "/agents/coder/agent.leviath".to_string(),
            workdir: "/work".to_string(),
            yolo: true,
        }
    }

    #[test]
    fn spawn_report_without_json_is_the_sentence() {
        assert_eq!(spawn_report(&spawned(), false), "spawned coder-1");
    }

    #[test]
    fn spawn_report_with_json_round_trips_every_field() {
        // Parsing it back is the assertion that matters: a caller reads this to
        // learn the id it has to poll, so the keys are the contract.
        let parsed: SpawnedRun =
            serde_json::from_str(&spawn_report(&spawned(), true)).expect("valid JSON");
        assert_eq!(parsed, spawned());
    }

    // ─── the client-side [read_paths] warning ──────────────────────────

    /// A blueprint declaring one absolute read path, so the same entry
    /// compiles on every OS.
    fn read_paths_blueprint() -> leviath_runtime::spec::Blueprint {
        leviath_runtime::spec::manifest::parse_manifest(
            r#"
[agent]
name = "cto"
version = "0.1.0"
description = "test"

[stages.main]
mode = "autonomous"

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }

[read_paths]
allow = ["/data/runs"]
"#,
        )
        .expect("blueprint parses")
    }

    /// The point of warning here at all: the person who typed `lev run` learns
    /// the declaration is inert now, not at the first refused read.
    #[test]
    fn an_ungranted_declaration_warns_with_the_stanza_to_add() {
        let lines = spawn_warning_lines(
            &read_paths_blueprint(),
            &crate::config::Config::default(),
            std::path::Path::new("/work"),
        );
        let joined = lines.join("\n");
        assert!(joined.contains("agent 'cto'"), "{joined}");
        assert!(joined.contains("[agent_read_paths.cto]"), "{joined}");
        assert!(joined.contains(r#"allow = ["/data/runs"]"#), "{joined}");
    }

    /// A run started against a broken config file works, on settings that are
    /// not the ones on disk. Nothing said so before this.
    #[test]
    fn a_config_that_does_not_load_warns_once_before_the_run_starts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        std::fs::write(&path, "default_provider = \"anthropic\"\n").unwrap();
        assert!(
            broken_config_warning(&path).is_empty(),
            "a file that loads says nothing"
        );
        assert!(
            broken_config_warning(&dir.path().join("absent.toml")).is_empty(),
            "no config file means defaults, not a broken one"
        );

        std::fs::write(&path, "default_provider = \"anthropic\"\nbroken : :\n").unwrap();
        let joined = broken_config_warning(&path).join("\n");
        assert!(joined.starts_with("warning: "), "{joined}");
        assert!(joined.contains("does not load"), "{joined}");
        assert!(joined.contains("line 2, column 8"), "{joined}");
        assert!(
            joined.contains("last config that did"),
            "it says which config the run is actually on: {joined}"
        );
        assert!(
            joined.contains("nothing needs restarting"),
            "and what to do about it: {joined}"
        );
    }

    /// The wrapper the spawn path actually calls, driven against an isolated
    /// config so it never reads the developer's real `~/.leviath`.
    #[test]
    fn the_spawn_path_warning_reads_the_configured_path() {
        crate::config::with_isolated_config_path("client-broken-config", |dir| {
            std::fs::write(dir.join("config.toml"), "broken : :").unwrap();
            // Prints to stderr; the wording is asserted above, and this is
            // about the wrapper reaching the right file.
            warn_broken_config();
            assert!(
                !broken_config_warning(&crate::config::Config::config_path()).is_empty(),
                "the isolated config is what it read"
            );
        });
    }

    #[test]
    fn a_granted_declaration_says_nothing() {
        let mut config = crate::config::Config::default();
        config.security.read_paths = vec!["/data/runs".to_string()];
        assert!(
            spawn_warning_lines(
                &read_paths_blueprint(),
                &config,
                std::path::Path::new("/work")
            )
            .is_empty()
        );
    }

    /// No declaration, nothing to say - and a config whose own grant list is
    /// broken is the daemon's error to report, not a warning to guess at.
    #[test]
    fn nothing_to_warn_about_produces_no_lines() {
        let plain = leviath_runtime::spec::manifest::parse_manifest(
            &crate::test_support::inline_coder_manifest(),
        )
        .expect("blueprint parses");
        assert!(
            spawn_warning_lines(
                &plain,
                &crate::config::Config::default(),
                std::path::Path::new("/work")
            )
            .is_empty()
        );

        let mut broken = crate::config::Config::default();
        broken.security.read_paths = vec!["regex:relative/.*".to_string()];
        assert!(
            spawn_warning_lines(
                &read_paths_blueprint(),
                &broken,
                std::path::Path::new("/work")
            )
            .is_empty()
        );
    }

    /// End to end over the real files: a manifest on disk plus an isolated
    /// config that grants nothing.
    #[tokio::test]
    async fn the_warning_reads_the_manifest_and_the_active_config() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            crate::test_support::inline_coder_manifest()
                + "\n[read_paths]\nallow = [\"/data/runs\"]\n",
        )
        .unwrap();
        let args = LocalRun {
            manifest: std::path::PathBuf::from(manifest.to_string_lossy().into_owned()),
            workdir: dir.path().to_string_lossy().into_owned(),
            ..LocalRun::default()
        };
        let lines = crate::config::with_isolated_config_path_async(
            "spawn-warn-read-paths",
            |_fake| async move {
                let lines = read_path_warning_for_spawn(&args);
                warn_ungranted_read_paths(&args);
                lines
            },
        )
        .await;
        let joined = lines.join("\n");
        assert!(joined.contains("1 declared, 0 granted"), "{joined}");
        assert!(joined.contains("[agent_read_paths.coder]"), "{joined}");
    }

    /// Every way the warning can decline to run: a manifest that will not
    /// parse, and a config that will not load. Neither may stop a spawn.
    #[test]
    fn the_warning_gives_up_quietly_on_a_broken_manifest_or_config() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, "not valid toml [[[").unwrap();
        assert!(
            read_path_warning_for_spawn(&LocalRun {
                manifest: std::path::PathBuf::from(manifest.to_string_lossy().into_owned()),
                ..LocalRun::default()
            })
            .is_empty()
        );

        std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();
        crate::config::with_isolated_config_path("spawn-warn-broken-config", |fake_dir| {
            std::fs::write(fake_dir.join("config.toml"), "not = valid = toml").unwrap();
            assert!(
                read_path_warning_for_spawn(&LocalRun {
                    manifest: std::path::PathBuf::from(manifest.to_string_lossy().into_owned()),
                    ..LocalRun::default()
                })
                .is_empty()
            );
        });
    }

    /// A manifest that declares a held checkpoint, written to disk, so the
    /// warning is exercised through the real read-and-parse path.
    fn manifest_with_a_held_checkpoint(dir: &std::path::Path) -> String {
        let manifest = dir.join("agent.leviath");
        std::fs::write(
            &manifest,
            r#"
[agent]
name = "held"
version = "0.1.0"
description = "holds a checkpoint"
entry_stage = "plan"

[stages.plan]
mode = "interactive_points"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
max_iterations = 5
available_tools = ["read_file"]

[[stages.plan.interaction_points]]
name = "plan_approval"
prompt = "Review the plan"
style = "confirm"
unattended = "ask"

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
"#,
        )
        .unwrap();
        manifest.to_string_lossy().into_owned()
    }

    /// `--yolo` reads as "run without me", so a run that stops anyway has to say
    /// so before it starts rather than look like a hang twenty minutes in.
    #[test]
    fn a_yolo_spawn_announces_the_checkpoints_that_still_hold() {
        let dir = tempfile::tempdir().unwrap();
        let blueprint_path = manifest_with_a_held_checkpoint(dir.path());
        crate::config::with_isolated_config_path("spawn-warn-held", |_fake| {
            let args = LocalRun {
                manifest: std::path::PathBuf::from(blueprint_path.clone()),
                yolo: true,
                ..LocalRun::default()
            };
            let joined = held_checkpoint_warning_for_spawn(&args).join("\n");
            assert!(joined.contains("plan: plan_approval"), "{joined}");
            warn_held_checkpoints(&args);

            // An attended run stops for a person everywhere, so there is nothing
            // to announce.
            assert!(
                held_checkpoint_warning_for_spawn(&LocalRun {
                    manifest: std::path::PathBuf::from(blueprint_path.clone()),
                    yolo: false,
                    ..LocalRun::default()
                })
                .is_empty()
            );
        });
    }

    /// The same three lenient arms as the read-path warning: a manifest that is
    /// not there, one that will not parse, and a config that will not load.
    /// None of them may stop a spawn.
    #[test]
    fn the_held_checkpoint_warning_gives_up_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.leviath");
        assert!(
            held_checkpoint_warning_for_spawn(&LocalRun {
                manifest: std::path::PathBuf::from(missing.to_string_lossy().into_owned()),
                yolo: true,
                ..LocalRun::default()
            })
            .is_empty()
        );

        let unparseable = dir.path().join("agent.leviath");
        std::fs::write(&unparseable, "not valid toml [[[").unwrap();
        assert!(
            held_checkpoint_warning_for_spawn(&LocalRun {
                manifest: std::path::PathBuf::from(unparseable.to_string_lossy().into_owned()),
                yolo: true,
                ..LocalRun::default()
            })
            .is_empty()
        );

        // A config that will not load reads as no deadline, which is also the
        // default: the checkpoints still hold, and naming them matters more
        // than naming a timeout the operator may not have set.
        let held = manifest_with_a_held_checkpoint(dir.path());
        crate::config::with_isolated_config_path("spawn-held-broken-config", |fake_dir| {
            std::fs::write(fake_dir.join("config.toml"), "not = valid = toml").unwrap();
            let joined = held_checkpoint_warning_for_spawn(&LocalRun {
                manifest: std::path::PathBuf::from(held.clone()),
                yolo: true,
                ..LocalRun::default()
            })
            .join("\n");
            assert!(joined.contains("plan_approval"), "{joined}");
            assert!(joined.contains("until somebody answers"), "{joined}");
        });
    }

    /// A manifest declaring a Rhai validator for its output, written to disk,
    /// so the retirement warning is exercised through the real read-and-parse
    /// path.
    fn manifest_with_a_validator(dir: &std::path::Path) -> String {
        let manifest = dir.join("agent.leviath");
        std::fs::write(
            &manifest,
            r#"
[agent]
name = "checked"
version = "0.1.0"
description = "declares a validator"

[agent.output]
format = "markdown"
validator = "checks/report.rhai"

[stages.plan]
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
"#,
        )
        .unwrap();
        manifest.to_string_lossy().into_owned()
    }

    /// A request whose only content is a format label, the shape every
    /// `--output-format` flag arrives in.
    fn format_request(label: &str) -> Option<leviath_core::output::OutputSpec> {
        Some(leviath_core::output::OutputSpec {
            format: Some(label.to_string()),
            ..leviath_core::output::OutputSpec::default()
        })
    }

    /// `--output-format` over a blueprint with a validator retires the check,
    /// and stderr now says so before the run starts; re-stating the declared
    /// format retires nothing and stays quiet, as does no override at all.
    #[test]
    fn an_output_format_override_announces_the_retired_checks() {
        let dir = tempfile::tempdir().unwrap();
        let blueprint_path = manifest_with_a_validator(dir.path());
        let args = LocalRun {
            manifest: std::path::PathBuf::from(blueprint_path.clone()),
            output: format_request("json"),
            ..LocalRun::default()
        };
        let joined = retired_check_warning_for_spawn(&args).join("\n");
        assert!(joined.contains("checks/report.rhai"), "{joined}");
        assert!(joined.contains("stage 'plan'"), "{joined}");
        assert!(joined.contains("'json'"), "{joined}");
        warn_retired_output_checks(&args);

        assert!(
            retired_check_warning_for_spawn(&LocalRun {
                manifest: std::path::PathBuf::from(blueprint_path.clone()),
                output: format_request("markdown"),
                ..LocalRun::default()
            })
            .is_empty(),
            "re-stating the declared format keeps the checks"
        );
        assert!(
            retired_check_warning_for_spawn(&LocalRun {
                manifest: std::path::PathBuf::from(blueprint_path),
                output: None,
                ..LocalRun::default()
            })
            .is_empty(),
            "no override, nothing retired"
        );
    }

    /// The same lenient arms as every warning on this path: a manifest that is
    /// not there or will not parse must stay quiet, never stop a spawn.
    #[test]
    fn the_retirement_warning_gives_up_quietly() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            retired_check_warning_for_spawn(&LocalRun {
                manifest: dir.path().join("nope.leviath"),
                output: format_request("json"),
                ..LocalRun::default()
            })
            .is_empty()
        );

        let unparseable = dir.path().join("agent.leviath");
        std::fs::write(&unparseable, "not valid toml [[[").unwrap();
        assert!(
            retired_check_warning_for_spawn(&LocalRun {
                manifest: std::path::PathBuf::from(unparseable.to_string_lossy().into_owned()),
                output: format_request("json"),
                ..LocalRun::default()
            })
            .is_empty()
        );
    }

    #[tokio::test]
    async fn send_spawn_reports_success() {
        assert!(
            send(r#"{"result":"spawned","run_id":"run-9"}"#)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn send_spawn_reports_daemon_error() {
        let err = send(r#"{"result":"error","message":"boom"}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn send_spawn_reports_a_refusal_with_its_issues() {
        let err = send(crate::test_support::rejected_reply("no such region"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no such region"), "{err}");
    }

    /// A flag the request cannot hold is refused before the daemon is asked.
    #[test]
    fn a_model_that_does_not_read_is_refused_locally() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_manifest(dir.path());
        let err = run_request(RunLine {
            path: manifest.to_str().unwrap(),
            task: Some("t"),
            stdin_is_terminal: &never_interactive,
            model: Some("not a model".to_string()),
            workdir: "/work",
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
            parts: Vec::new(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("not a model"), "{err}");
    }

    #[tokio::test]
    async fn send_spawn_reports_unexpected_response() {
        let err = send(r#"{"result":"ok","ok":true}"#).await.unwrap_err();
        assert!(err.to_string().contains("unexpected"));
    }

    #[tokio::test]
    async fn send_spawn_errors_when_daemon_absent() {
        let dir = tempfile::tempdir().unwrap();
        // A control id with no daemon bound to it.
        let id = control_id(&dir.path().join("no-daemon"));
        let err = send_spawn(&ControlClient::new(id), LocalRun::default(), false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not reachable"));
    }

    /// `--yolo=<name>` reaches the daemon as the bit plus the name.
    #[test]
    fn run_request_carries_a_yolo_profile() {
        let root = tempfile::tempdir().unwrap();
        let agent_dir = root.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let manifest = write_manifest(&agent_dir);
        let args = run_request(RunLine {
            parts: Vec::new(),
            path: manifest.to_str().unwrap(),
            task: Some("do it"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: true,
            yolo_profile: Some("careful".to_string()),
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
        })
        .unwrap();
        assert!(args.yolo);
        assert_eq!(args.yolo_profile.as_deref(), Some("careful"));
    }

    /// The profile pre-flight: nothing for an attended run or the bare flag,
    /// what a named profile keeps for a person, and a refusal for a name the
    /// file does not have - before the daemon is asked.
    #[tokio::test]
    async fn the_yolo_profile_preflight_names_holds_and_refuses_unknown_names() {
        crate::config::with_isolated_config_path_async("spawn-yolo-profile", |cfg| async move {
            std::fs::write(
                cfg.join("yolo.toml"),
                "[careful]\ndefault = \"ask\"\nquestions = \"ask\"\n\n[loose]\ndefault = \"allow\"\n",
            )
            .unwrap();
            let with = |yolo: bool, profile: Option<&str>| LocalRun {
                yolo,
                yolo_profile: profile.map(str::to_string),
                ..LocalRun::default()
            };
            let preflight =
                |run: &LocalRun| yolo_profile_preflight(run.yolo, run.yolo_profile.as_deref());
            assert!(preflight(&with(false, None)).unwrap().is_empty());
            assert!(preflight(&with(true, None)).unwrap().is_empty());
            assert!(preflight(&with(false, Some("nope"))).unwrap().is_empty());

            let lines = preflight(&with(true, Some("careful"))).unwrap();
            assert_eq!(lines[0], "--yolo=careful keeps these for you:");
            assert!(lines[1].contains("questions"), "{lines:?}");
            assert!(lines[2].contains("lists do not allow"), "{lines:?}");

            let lines = preflight(&with(true, Some("loose"))).unwrap();
            assert_eq!(lines.len(), 1);
            assert!(lines[0].contains("keeps nothing for you"), "{lines:?}");

            let err = preflight(&with(true, Some("nope"))).unwrap_err();
            assert!(err.to_string().contains("careful, loose"), "{err}");

            // Through `send_spawn`: refused before any socket is dialled, so
            // the error is the profile's, not "daemon not reachable".
            let id = control_id(&cfg.join("nowhere"));
            let err = send_spawn(&ControlClient::new(id), with(true, Some("nope")), false)
                .await
                .expect_err("an unknown profile stops the spawn");
            assert!(err.to_string().contains("no yolo profile"), "{err}");
        })
        .await;
    }

    /// A profiled spawn prints what the profile keeps before the daemon is
    /// asked, on the single path and the batch path, and an unknown name
    /// stops a batch before its first run.
    #[tokio::test]
    async fn a_profiled_spawn_prints_its_holds_before_the_daemon_is_asked() {
        crate::config::with_isolated_config_path_async("spawn-yolo-holds", |cfg| async move {
            std::fs::write(
                cfg.join("yolo.toml"),
                "[careful]\ndefault = \"ask\"\nquestions = \"ask\"\n",
            )
            .unwrap();
            let args = LocalRun {
                yolo: true,
                yolo_profile: Some("careful".to_string()),
                ..LocalRun::default()
            };
            let one = tempfile::tempdir().unwrap();
            let (id, server) = fake_daemon_serving(
                one.path(),
                vec![r#"{"result":"spawned","run_id":"a-1-000000000001"}"#],
            );
            send_spawn(&ControlClient::new(id), args.clone(), false)
                .await
                .expect("one spawn");
            server.await.unwrap();

            let many = tempfile::tempdir().unwrap();
            let (id, server) = fake_daemon_serving(
                many.path(),
                vec![
                    r#"{"result":"spawned","run_id":"a-1-000000000002"}"#,
                    r#"{"result":"spawned","run_id":"a-1-000000000003"}"#,
                ],
            );
            send_spawn_batch(&ControlClient::new(id), args.clone(), 2, false)
                .await
                .expect("a batch");
            server.await.unwrap();

            let none = tempfile::tempdir().unwrap();
            let id = control_id(&none.path().join("no-daemon"));
            let err = send_spawn_batch(
                &ControlClient::new(id),
                LocalRun {
                    yolo_profile: Some("nope".to_string()),
                    ..args
                },
                2,
                false,
            )
            .await
            .expect_err("an unknown profile stops the batch");
            assert!(err.to_string().contains("no yolo profile"), "{err}");
        })
        .await;
    }
}

#[cfg(test)]
mod part_tests {
    use super::*;
    use leviath_core::mime::InboundPart;

    /// `run`'s task, or nothing.
    fn task_of(run: &LocalRun) -> String {
        serde_json::to_value(&run.request.inputs).unwrap()["task"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// The region an attachment names, as text.
    fn region_of(part: &leviath_runtime::spec::request::Attachment) -> Option<&str> {
        part.region.as_ref().map(|r| r.as_str())
    }

    fn write_typed_manifest(dir: &std::path::Path, regions: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("agent.leviath"),
            format!(
                "[agent]\nname = \"artist\"\n\n[stages.main]\nmode = \"autonomous\"\n\n\
                 [stages.main.model]\nprovider = \"anthropic\"\nmodel = \"claude-sonnet-5\"\n\n\
                 [context.regions]\n{regions}\n"
            ),
        )
        .unwrap();
        dir.join("agent.leviath")
    }

    const TASK_AND_ART: &str = "task = { kind = \"pinned\", max_tokens = 4000, seed = \"task_input\" }\n\
        art = { kind = \"pinned\", max_tokens = 4000, seed = \"input\", accepts = [\"image/*\"] }\n\
        conversation = { kind = \"sliding_window\", max_items = 20, max_tokens = 10000 }";

    fn request<'a>(
        manifest: &'a str,
        task: Option<&'a str>,
        regions: HashMap<String, String>,
        parts: Vec<InboundPart>,
    ) -> RunLine<'a> {
        RunLine {
            yolo_profile: None,
            path: manifest,
            task,
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
            parts,
        }
    }

    #[test]
    fn attached_named_and_region_files_all_become_parts() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_typed_manifest(&dir.path().join("artist"), TASK_AND_ART);
        let png = dir.path().join("hero.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nbody").unwrap();
        let task = format!(
            "edit @{} so the arm is longer, not @nothing.png",
            png.display()
        );
        let regions = HashMap::from([("art".to_string(), format!("@{}", png.display()))]);
        let attached = InboundPart::from_bytes("extra.wav", vec![1, 2, 3]);
        let args = run_request(request(
            manifest.to_str().unwrap(),
            Some(&task),
            regions,
            vec![attached],
        ))
        .unwrap();
        assert_eq!(task_of(&args), task);
        assert!(
            !args.request.inputs.contains_key("art"),
            "a binary region file is a part, not text"
        );
        let parts = &args.request.attachments;
        let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["hero.png", "extra.wav", "hero.png"]);
        assert_eq!(region_of(&parts[0]), Some("art"));
        assert_eq!(region_of(&parts[1]), None);
        assert_eq!(region_of(&parts[2]), None);
    }

    /// The dashboard resolves a task's `@path` into a part and still sends the
    /// task naming it, so the resolver finds the same file again; the exact
    /// repeat is dropped. A copy in another region, or a different file, stays.
    #[test]
    fn an_exact_repeat_part_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_typed_manifest(&dir.path().join("artist"), TASK_AND_ART);
        let png = dir.path().join("hero.png");
        let bytes = b"\x89PNG\r\n\x1a\nbody".to_vec();
        std::fs::write(&png, &bytes).unwrap();
        let task = format!("see @{}", png.display());
        let attached = InboundPart::from_bytes("hero.png", bytes.clone());
        let args = run_request(request(
            manifest.to_str().unwrap(),
            Some(&task),
            HashMap::new(),
            vec![attached],
        ))
        .unwrap();
        let names: Vec<&str> = args
            .request
            .attachments
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["hero.png"], "the exact repeat was dropped");

        // The same bytes attached to a named region is a different part, and a
        // different file is too: both are kept beside the task's copy.
        let other = dir.path().join("other.png");
        std::fs::write(&other, b"\x89PNG\r\n\x1a\nother").unwrap();
        let task2 = format!("see @{} and @{}", png.display(), other.display());
        let art = InboundPart::from_bytes("hero.png", bytes).in_region("art");
        let args = run_request(request(
            manifest.to_str().unwrap(),
            Some(&task2),
            HashMap::new(),
            vec![art],
        ))
        .unwrap();
        let mut got: Vec<(Option<&str>, &str)> = args
            .request
            .attachments
            .iter()
            .map(|p| (region_of(p), p.name.as_str()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                (None, "hero.png"),
                (None, "other.png"),
                (Some("art"), "hero.png"),
            ]
        );
    }

    /// A task naming an empty file is refused before anything is dialled.
    #[test]
    fn a_task_naming_an_empty_file_is_refused_before_dialling() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_typed_manifest(&dir.path().join("artist2"), TASK_AND_ART);
        let empty = dir.path().join("empty.png");
        std::fs::write(&empty, b"").unwrap();
        let task = format!("edit @{}", empty.display());
        let err = run_request(request(
            manifest.to_str().unwrap(),
            Some(&task),
            HashMap::new(),
            Vec::new(),
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("nothing to attach"), "{err}");
    }
}
