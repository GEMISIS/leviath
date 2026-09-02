//! Client-side helpers for talking to the shared-world daemon: building a spawn
//! request from local inputs and exchanging it over the control socket. Shared by
//! `lev run` (and reusable by other clients). The socket-path resolution + connect
//! live in the binary; these cores are unit-testable against a fake socket server.

use std::collections::HashMap;

use anyhow::bail;
use leviath_core::run_meta::RunStatus;
use leviath_runtime::control_socket::{ControlClient, ControlResponse};
use leviath_runtime::host::SpawnArgs;

use crate::commands::run::manifest::find_manifest;
use crate::commands::run::task::{read_region_value, resolve_task};
use crate::runstate::new_run_id;

/// Everything a spawn request needs from the agent's own files.
pub(crate) struct AgentSource {
    /// The resolved `agent.leviath` path.
    pub manifest: std::path::PathBuf,
    /// The manifest's parent directory name, which the run id is minted from.
    /// Deliberately not `blueprint.name`: the run id is what `lev ps` shows and
    /// what identifies the checkout on disk, while the blueprint's own name is
    /// what the agent calls itself.
    pub run_stem: String,
    /// The parsed blueprint itself.
    pub blueprint: leviath_core::Blueprint,
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
    let run_stem = manifest
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("agent")
        .to_string();
    let content = std::fs::read_to_string(&manifest)
        .map_err(|e| anyhow::anyhow!("read manifest '{}': {e}", manifest.display()))?;
    let blueprint = leviath_core::manifest::parse_manifest(&content)
        .map_err(|e| anyhow::anyhow!("parse manifest: {e}"))?;
    Ok(AgentSource {
        manifest,
        run_stem,
        blueprint,
    })
}

/// Validate and resolve the dynamic `--<region>` flag values against the
/// blueprint's declared caller-input regions.
///
/// An unknown region name (one the blueprint doesn't read as caller input) is a
/// hard error - fast, local typo protection before the daemon is contacted.
fn resolve_regions(
    blueprint: &leviath_core::Blueprint,
    regions: HashMap<String, String>,
) -> anyhow::Result<HashMap<String, String>> {
    let declared = blueprint.caller_inputs();
    let mut out = HashMap::new();
    for (name, raw) in regions {
        if !declared.contains(&name.as_str()) {
            bail!(
                "unknown region '--{name}'; this agent's caller-input regions are: {}",
                if declared.is_empty() {
                    "(none)".to_string()
                } else {
                    declared.join(", ")
                }
            );
        }
        out.insert(name, read_region_value(&raw)?);
    }
    Ok(out)
}

/// The stdin probe for callers that build a spawn request from inside the
/// daemon: fan-out workers and sub-agents. There is no terminal there, and an
/// editor launched from a background process would block it forever with
/// nobody to close the window.
///
/// Those callers always have a task in hand, so the probe is never actually
/// consulted; passing this rather than a bare `|| false` states the reason at
/// each call site.
pub(crate) fn never_interactive() -> bool {
    false
}

/// What `lev run` was asked for, before any of it is resolved.
///
/// One struct because these are one thing: the command line. Each field is a
/// flag the user typed, and grouping them keeps the difference between "what was
/// asked for" and "what that resolves to" visible - `resolve_spawn_args` turns
/// this into a [`SpawnArgs`], and the two are deliberately different types.
pub struct LaunchRequest<'a> {
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
    /// `--allow`: tools permitted outright.
    pub allow: Vec<String>,
    /// `--max-depth`: sub-agent tree cap.
    pub max_depth: Option<usize>,
    /// `--<region>` seeds, keyed by caller-input region name.
    pub regions: HashMap<String, String>,
    /// `--no-seed-commands`: refuse the blueprint's command seeds.
    pub no_seed_commands: bool,
    /// The output shape the caller asked for, overriding the blueprint's.
    pub output_request: Option<leviath_core::output::OutputSpec>,
}

/// Resolve the local inputs of a spawn request: find and parse the manifest,
/// resolve the `--<region>` flags, resolve the task, and mint a run id from the
/// agent's directory name.
///
/// `task` is what `--task` was given, if anything. Left off, `resolve_task`
/// opens the user's editor, which is why `stdin_is_terminal` is threaded
/// through: the probe itself is real I/O and belongs to the binary, so callers
/// inject it (tests pass a `fn` that always says no). None of that happens for a
/// blueprint that takes no task: it is not asked for one, and giving it one is
/// an error rather than text with nowhere to go.
///
/// Regions are resolved *before* the task on purpose. A typo'd `--foo` has to
/// fail before the user is dropped into an editor and types a paragraph they
/// are about to lose.
pub fn resolve_spawn_args(req: LaunchRequest<'_>) -> anyhow::Result<SpawnArgs> {
    let LaunchRequest {
        path,
        task,
        stdin_is_terminal,
        model,
        workdir,
        yolo,
        allow,
        max_depth,
        regions,
        no_seed_commands,
        output_request,
    } = req;
    let source = load_agent_source(path)?;
    let resolved_regions = resolve_regions(&source.blueprint, regions)?;
    // An agent driven by named regions takes no task, so neither demanding one
    // nor opening an editor to write one would make sense - `lev run reviewer
    // --diff @x.patch` is a complete command line. Handing it one anyway is the
    // error, and it is the same message the daemon would give.
    let task = match source.blueprint.accepts_task() {
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

    Ok(SpawnArgs {
        run_id: new_run_id(&source.run_stem),
        blueprint_path: source.manifest.to_string_lossy().to_string(),
        task,
        regions: resolved_regions,
        model,
        workdir: workdir.to_string(),
        metadata: Default::default(),
        callback_url: None,
        callback_secret: None,
        yolo,
        no_seed_commands,
        allow,
        max_depth,
        // A top-level run (sub-agents/fan-out set this on the host side).
        parent_run_id: None,
        output: output_request,
    })
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
fn warn_ungranted_read_paths(spawn_args: &SpawnArgs) {
    for line in read_path_warning_for_spawn(spawn_args) {
        eprintln!("{line}");
    }
}

/// The warning for a spawn request, read from the real manifest and config.
/// Empty when there is nothing to say, and empty when either file cannot be
/// read: see [`warn_ungranted_read_paths`] for why that is not an error here.
pub(crate) fn read_path_warning_for_spawn(spawn_args: &SpawnArgs) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(&spawn_args.blueprint_path) else {
        return Vec::new();
    };
    let Ok(blueprint) = leviath_core::manifest::parse_manifest(&content) else {
        return Vec::new();
    };
    let Ok(config) = crate::config::Config::load() else {
        return Vec::new();
    };
    spawn_warning_lines(
        &blueprint,
        &config,
        std::path::Path::new(&spawn_args.workdir),
    )
}

/// The warning itself: one line saying what is refused, then the stanza that
/// would grant it. Pure, so the wording is testable without a daemon.
fn spawn_warning_lines(
    blueprint: &leviath_core::Blueprint,
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

/// Say, before the run starts, that `--yolo` will still stop for a person.
///
/// `--yolo` means "run without me", so a run that stops anyway reads as a hang.
/// The daemon does lint the blueprint at spawn, but only into `daemon.log`,
/// which the person typing the command never sees.
///
/// Best-effort for the same reason as [`warn_ungranted_read_paths`]: an
/// unreadable manifest or config is the daemon's to report, and this must never
/// be why a run does not start.
fn warn_held_checkpoints(spawn_args: &SpawnArgs) {
    for line in held_checkpoint_warning_for_spawn(spawn_args) {
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
pub(crate) fn held_checkpoint_warning_for_spawn(spawn_args: &SpawnArgs) -> Vec<String> {
    let path = std::path::Path::new(&spawn_args.blueprint_path);
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(blueprint) = leviath_core::manifest::parse_manifest(&content) else {
        return Vec::new();
    };
    let mut lines: Vec<String> =
        crate::bundled::stale_install_note(path, &blueprint, leviath_core::agents_dir().as_deref())
            .into_iter()
            .collect();
    if spawn_args.yolo {
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
fn warn_retired_output_checks(spawn_args: &SpawnArgs) {
    for line in retired_check_warning_for_spawn(spawn_args) {
        eprintln!("warning: {line}");
    }
}

/// The retirement warning for a spawn request, read from the real manifest.
/// Empty when nothing is retired, and empty when the manifest cannot be read
/// or parsed: best-effort for the same reason as
/// [`warn_ungranted_read_paths`].
fn retired_check_warning_for_spawn(spawn_args: &SpawnArgs) -> Vec<String> {
    if spawn_args.output.is_none() {
        return Vec::new();
    }
    let Ok(content) = std::fs::read_to_string(&spawn_args.blueprint_path) else {
        return Vec::new();
    };
    let Ok(blueprint) = leviath_core::manifest::parse_manifest(&content) else {
        return Vec::new();
    };
    leviath_core::output::retired_check_warnings(&blueprint, spawn_args.output.as_ref())
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

/// A fresh run id for the same agent as `previous`.
///
/// Ids are minted `<stem>-<secs>-<hex12>` (see [`crate::runstate::new_run_id`]),
/// so the stem is everything before the last two dash-separated components.
/// The stem itself may contain dashes (`wide-researcher`), which is why this
/// strips from the right. An id that does not have the minted shape is used as
/// the stem wholesale - a fresh unique id still comes out.
fn respawned_run_id(previous: &str) -> String {
    let mut parts = previous.rsplitn(3, '-');
    let _entropy = parts.next();
    let _secs = parts.next();
    let stem = parts.next().unwrap_or(previous);
    crate::runstate::new_run_id(stem)
}

/// Send a resolved spawn request to the daemon and report the outcome, printing
/// the new run id on success.
///
/// Warnings go to stderr, so `--json` leaves stdout parseable on its own.
pub(crate) async fn send_spawn(
    client: &ControlClient,
    spawn_args: SpawnArgs,
    json: bool,
) -> anyhow::Result<()> {
    warn_before_spawn(&spawn_args);
    let spawned = spawn_once(client, spawn_args).await?;
    println!("{}", spawn_report(&spawned, json));
    Ok(())
}

/// Send `count` copies of a resolved spawn request - the same agent, task, and
/// flags, each under its own fresh run id - and print one combined report.
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
    spawn_args: SpawnArgs,
    count: usize,
    json: bool,
) -> anyhow::Result<()> {
    if count == 0 {
        bail!("--count must be at least 1");
    }
    if count == 1 {
        return send_spawn(client, spawn_args, json).await;
    }
    // The warnings describe the blueprint and the machine, not the individual
    // run: once.
    warn_before_spawn(&spawn_args);
    let mut spawned = Vec::with_capacity(count);
    for _ in 0..count {
        let mut args = spawn_args.clone();
        args.run_id = respawned_run_id(&spawn_args.run_id);
        match spawn_once(client, args).await {
            Ok(run) => spawned.push(run),
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
///
/// `pub(crate)` for the MCP server, which prints nothing on stdout and reports
/// the outcome in its own shape.
pub(crate) async fn spawn_once(
    client: &ControlClient,
    spawn_args: SpawnArgs,
) -> anyhow::Result<SpawnedRun> {
    let blueprint_path = spawn_args.blueprint_path.clone();
    let workdir = spawn_args.workdir.clone();
    let yolo = spawn_args.yolo;
    match client.spawn(spawn_args).await {
        Ok(ControlResponse::Spawned { run_id }) => Ok(SpawnedRun {
            run_id,
            blueprint_path,
            workdir,
            yolo,
        }),
        Ok(ControlResponse::Error { message }) => bail!("spawn failed: {message}"),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
    }
}

/// Print every pre-flight warning for a spawn request, on stderr.
fn warn_before_spawn(spawn_args: &SpawnArgs) {
    warn_broken_config();
    warn_ungranted_read_paths(spawn_args);
    warn_held_checkpoints(spawn_args);
    warn_retired_output_checks(spawn_args);
}

/// `--wait` follows one run, so a batch cannot be waited on: refuse the
/// combination before anything is spawned rather than wait on the first run
/// and leave the others unmentioned.
pub fn refuse_wait_with_count(count: usize, wait: bool) -> anyhow::Result<()> {
    if wait && count > 1 {
        bail!("--wait follows a single run; drop it or use --count 1");
    }
    Ok(())
}

/// `lev run --wait`: spawn the run and stay until it finishes, then print its
/// answer once.
///
/// One composition here rather than in the binary, because the pieces it joins
/// (`spawn_once`, the wait loop, `lev result`'s rendering) are crate-private
/// and because `send_spawn` prints the spawn report: with `--json` that would
/// put two JSON documents on stdout. Exactly one thing is printed: with `json`
/// the object `{run_id, status, final_output}`, otherwise what
/// `lev result <run-id>` prints (or one line saying there is no answer).
/// Returns `Err` when the run ended in error or was cancelled, when it parked
/// on a question nobody at this terminal is being asked, or when the daemon
/// could not be followed, so the exit code says how it went.
pub async fn spawn_and_wait(
    client: &ControlClient,
    spawn_args: SpawnArgs,
    json: bool,
    runs_dir: &std::path::Path,
) -> anyhow::Result<()> {
    spawn_and_wait_with(client, spawn_args, json, runs_dir, None).await
}

/// [`spawn_and_wait`] with a deadline on the wait, after which the run is left
/// running and the command fails saying so. `lev run --wait` has no deadline
/// flag today; the parameter exists so the deadline arm is a tested path
/// rather than a dead one.
pub(crate) async fn spawn_and_wait_with(
    client: &ControlClient,
    spawn_args: SpawnArgs,
    json: bool,
    runs_dir: &std::path::Path,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<()> {
    warn_before_spawn(&spawn_args);
    // Subscribe before spawning so no event between the two is missed.
    let stream = match client.subscribe().await {
        Ok(stream) => stream,
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
    };
    let spawned = spawn_once(client, spawn_args).await?;
    let run_id = spawned.run_id;
    let outcome = crate::daemon::wait::wait_for_run(
        client,
        stream,
        &run_id,
        runs_dir,
        timeout,
        &mut |_event| {},
    )
    .await;
    use crate::daemon::wait::WaitOutcome;
    match outcome {
        WaitOutcome::Finished {
            status,
            final_output,
            error,
            ..
        } => {
            let text = match json {
                true => {
                    let report = serde_json::json!({
                        "run_id": run_id,
                        "status": status.wire(),
                        "final_output": final_output,
                    });
                    format!(
                        "{}\n",
                        serde_json::to_string_pretty(&report).expect("a report serializes")
                    )
                }
                false => {
                    crate::commands::result::render(&run_id, final_output.as_ref(), false, false)
                        .unwrap_or_else(|| {
                            format!(
                                "run '{run_id}' finished with status {status} and no final output\n"
                            )
                        })
                }
            };
            print!("{text}");
            match status {
                RunStatus::Error | RunStatus::Cancelled => bail!(
                    "run '{run_id}' ended with status {status}{}",
                    error.map(|e| format!(": {e}")).unwrap_or_default()
                ),
                _ => Ok(()),
            }
        }
        WaitOutcome::Interaction {
            run_id: asker,
            request,
        } => bail!(
            "run '{asker}' is waiting for an answer to interaction '{}' ({}); answer it with \
             `lev respond {}` and read the result with `lev result {run_id}`",
            request.id,
            request.prompt,
            request.id
        ),
        WaitOutcome::TimedOut { .. } => bail!("stopped waiting for run '{run_id}'; it continues"),
        WaitOutcome::Lost { reason } => {
            bail!("lost track of run '{run_id}' ({reason}); it may still be running - see `lev ps`")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::control_socket::{ControlId, bind_control_listener, control_id};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::task::JoinHandle;

    fn write_manifest(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::write(
            dir.join("agent.leviath"),
            crate::test_support::inline_coder_manifest(),
        )
        .unwrap();
        dir.join("agent.leviath")
    }

    #[test]
    fn resolve_spawn_args_finds_manifest_and_builds_request() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let manifest = write_manifest(&agent_dir);

        let args = resolve_spawn_args(LaunchRequest {
            path: manifest.to_str().unwrap(),
            task: Some("do it"),
            stdin_is_terminal: &never_interactive,
            model: Some("m".to_string()),
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
        })
        .unwrap();
        assert!(args.run_id.contains("my-agent"));
        assert_eq!(args.task, "do it");
        assert_eq!(args.model.as_deref(), Some("m"));
        assert_eq!(
            args.blueprint_path,
            std::fs::canonicalize(&manifest).unwrap().to_string_lossy()
        );
        assert_eq!(args.workdir, "/work");
    }

    /// The daemon has its own working directory, so a relative `PATH` has to be
    /// resolved before the request leaves: `lev run .` reaching the daemon as
    /// `./agent.leviath` fails there, and it is the very command `lev create`
    /// prints as the next step.
    #[test]
    fn resolve_spawn_args_sends_an_absolute_blueprint_path_for_a_relative_input() {
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

        let args = resolve_spawn_args(LaunchRequest {
            path: relative.to_str().unwrap(),
            task: Some("do it"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
        })
        .unwrap();
        assert!(
            std::path::Path::new(&args.blueprint_path).is_absolute(),
            "got: {}",
            args.blueprint_path
        );
        assert!(args.blueprint_path.ends_with("agent.leviath"));
    }

    #[test]
    fn resolve_spawn_args_errors_on_missing_manifest() {
        assert!(
            resolve_spawn_args(LaunchRequest {
                path: "/no/such/agent",
                task: Some("t"),
                stdin_is_terminal: &never_interactive,
                model: None,
                workdir: "/work",
                yolo: false,
                allow: Vec::new(),
                max_depth: None,
                regions: HashMap::new(),
                no_seed_commands: false,
                output_request: None,
            })
            .is_err()
        );
    }

    /// `--task <file>` end to end through the real wiring, not just through
    /// `resolve_task` in isolation.
    #[test]
    fn resolve_spawn_args_reads_the_task_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let manifest = write_manifest(&agent_dir);
        let task_file = dir.path().join("task.md");
        std::fs::write(&task_file, "  summarize the README  \n").unwrap();

        let args = resolve_spawn_args(LaunchRequest {
            path: manifest.to_str().unwrap(),
            task: Some(task_file.to_str().unwrap()),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
        })
        .unwrap();
        assert_eq!(args.task, "summarize the README");
    }

    /// No `--task` and no terminal to open an editor on: the run is refused
    /// here, before the daemon is contacted.
    #[test]
    fn resolve_spawn_args_without_a_task_errors_when_stdin_is_not_a_tty() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let manifest = write_manifest(&agent_dir);

        let err = resolve_spawn_args(LaunchRequest {
            path: manifest.to_str().unwrap(),
            task: None,
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
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

    /// `lev run diffonly --diff ...` is a complete command line, so no task is
    /// demanded and no editor is opened - which is the whole reason the demand
    /// is conditional rather than unconditional.
    #[test]
    fn an_agent_that_takes_no_task_is_not_asked_for_one() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_taskless_manifest(&dir.path().join("diffonly"));
        let mut regions = HashMap::new();
        regions.insert("diff".to_string(), "a patch".to_string());

        let args = resolve_spawn_args(LaunchRequest {
            path: manifest.to_str().unwrap(),
            task: None,
            // Says stdin is not a TTY, so an unconditional demand would error
            // here rather than fall through to the editor.
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
        })
        .expect("no task is required of an agent that takes none");
        assert_eq!(args.task, "");
        assert_eq!(
            args.regions.get("diff").map(String::as_str),
            Some("a patch")
        );
    }

    /// The other half: handing that agent a task is the error, and the message
    /// points at the input it does take.
    #[test]
    fn an_agent_that_takes_no_task_refuses_one() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_taskless_manifest(&dir.path().join("diffonly"));

        let err = resolve_spawn_args(LaunchRequest {
            path: manifest.to_str().unwrap(),
            task: Some("review my code"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
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

        let args = resolve_spawn_args(LaunchRequest {
            path: manifest.to_str().unwrap(),
            task: Some("   "),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions: HashMap::new(),
            no_seed_commands: false,
            output_request: None,
        })
        .expect("blank is the same as absent");
        assert_eq!(args.task, "");
    }

    /// Pins the ordering: a typo'd region flag must fail *before* the user is
    /// dropped into an editor, or they type a paragraph and then lose it.
    #[test]
    fn resolve_spawn_args_rejects_a_bad_region_before_it_looks_at_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_region_manifest(&dir.path().join("reviewer"));
        let regions = HashMap::from([("bogus".to_string(), "x".to_string())]);

        let err = resolve_spawn_args(LaunchRequest {
            path: manifest.to_str().unwrap(),
            task: None,
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
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
    fn resolve_spawn_args_resolves_declared_region_and_reads_at_path() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_region_manifest(&dir.path().join("reviewer"));
        let policy = dir.path().join("policy.md");
        std::fs::write(&policy, "  focus on safety  ").unwrap();

        let regions = HashMap::from([(
            "criteria".to_string(),
            format!("@{}", policy.to_string_lossy()),
        )]);
        let args = resolve_spawn_args(LaunchRequest {
            path: manifest.to_str().unwrap(),
            task: Some("review it"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
        })
        .unwrap();
        // `@path` was read and trimmed.
        assert_eq!(
            args.regions.get("criteria").map(String::as_str),
            Some("focus on safety")
        );
    }

    #[test]
    fn resolve_spawn_args_unknown_region_reports_none_when_no_caller_inputs() {
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
        let err = resolve_spawn_args(LaunchRequest {
            path: manifest.to_str().unwrap(),
            task: Some("t"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("(none)"), "got: {err}");
    }

    #[test]
    fn resolve_spawn_args_manifest_read_error_surfaces() {
        // `find_manifest` accepts a dir whose `agent.leviath` merely *exists*; when
        // that entry is itself a directory, the client-side read fails (EISDIR).
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("dirmanifest");
        std::fs::create_dir_all(agent_dir.join("agent.leviath")).unwrap();
        let regions = HashMap::from([("x".to_string(), "y".to_string())]);
        let err = resolve_spawn_args(LaunchRequest {
            path: agent_dir.to_str().unwrap(),
            task: Some("t"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("read manifest"), "got: {err}");
    }

    #[test]
    fn resolve_spawn_args_manifest_parse_error_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("badtoml");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("agent.leviath"),
            "this is : not = valid toml [[[",
        )
        .unwrap();
        let regions = HashMap::from([("x".to_string(), "y".to_string())]);
        let err = resolve_spawn_args(LaunchRequest {
            path: agent_dir.join("agent.leviath").to_str().unwrap(),
            task: Some("t"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("parse manifest"), "got: {err}");
    }

    #[test]
    fn resolve_spawn_args_region_value_bad_file_errors() {
        // A declared region whose `@file` value can't be read → the error from
        // read_region_value propagates out of resolve_spawn_args.
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_region_manifest(&dir.path().join("reviewer"));
        let regions = HashMap::from([("criteria".to_string(), "@/no/such/file.md".to_string())]);
        let err = resolve_spawn_args(LaunchRequest {
            path: manifest.to_str().unwrap(),
            task: Some("review it"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("Failed to read region file"),
            "got: {err}"
        );
    }

    #[test]
    fn resolve_spawn_args_rejects_unknown_region_flag() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_region_manifest(&dir.path().join("reviewer"));
        let regions = HashMap::from([("bogus".to_string(), "x".to_string())]);
        let err = resolve_spawn_args(LaunchRequest {
            path: manifest.to_str().unwrap(),
            task: Some("review it"),
            stdin_is_terminal: &never_interactive,
            model: None,
            workdir: "/work",
            yolo: false,
            allow: Vec::new(),
            max_depth: None,
            regions,
            no_seed_commands: false,
            output_request: None,
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
        let result = send_spawn(&ControlClient::new(id), SpawnArgs::default(), false).await;
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
        let args = SpawnArgs {
            run_id: "wide-researcher-1785900000-0123456789ab".to_string(),
            ..SpawnArgs::default()
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
        let err = send_spawn_batch(&ControlClient::new(id), SpawnArgs::default(), 3, false)
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
        send_spawn_batch(&ControlClient::new(id), SpawnArgs::default(), 1, false)
            .await
            .expect("the single spawn succeeds");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_batch_of_zero_is_refused_before_any_daemon_contact() {
        let dir = tempfile::tempdir().unwrap();
        // No listener bound: reaching the daemon at all would error differently.
        let id = control_id(dir.path());
        let err = send_spawn_batch(&ControlClient::new(id), SpawnArgs::default(), 0, false)
            .await
            .expect_err("zero runs is a refusal");
        assert!(err.to_string().contains("at least 1"), "got: {err}");
    }

    /// The stem survives its own dashes: only the minted `-<secs>-<hex>` tail
    /// is replaced.
    #[test]
    fn a_respawned_id_keeps_the_dashed_agent_stem() {
        let id = respawned_run_id("wide-researcher-1785900000-0123456789ab");
        assert!(id.starts_with("wide-researcher-"), "got: {id}");
        assert_ne!(id, "wide-researcher-1785900000-0123456789ab");
        // The minted shape holds: stem + seconds + 12 hex chars.
        let tail: Vec<&str> = id.rsplitn(3, '-').collect();
        assert_eq!(tail[0].len(), 12, "got: {id}");
        assert!(tail[1].chars().all(|c| c.is_ascii_digit()), "got: {id}");
    }

    /// An id without the minted tail is used as the stem wholesale - the
    /// result is still fresh and unique.
    #[test]
    fn a_respawned_id_falls_back_to_the_whole_previous_id_as_stem() {
        let id = respawned_run_id("x");
        assert!(id.starts_with("x-"), "got: {id}");
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
            run_id: "run-abc".to_string(),
            blueprint_path: "/agents/coder/agent.leviath".to_string(),
            workdir: "/work".to_string(),
            yolo: true,
        }
    }

    #[test]
    fn spawn_report_without_json_is_the_sentence() {
        assert_eq!(spawn_report(&spawned(), false), "spawned run-abc");
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
    fn read_paths_blueprint() -> leviath_core::Blueprint {
        leviath_core::manifest::parse_manifest(
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
        let plain =
            leviath_core::manifest::parse_manifest(&crate::test_support::inline_coder_manifest())
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
        let args = SpawnArgs {
            blueprint_path: manifest.to_string_lossy().into_owned(),
            workdir: dir.path().to_string_lossy().into_owned(),
            ..SpawnArgs::default()
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
            read_path_warning_for_spawn(&SpawnArgs {
                blueprint_path: manifest.to_string_lossy().into_owned(),
                ..SpawnArgs::default()
            })
            .is_empty()
        );

        std::fs::write(&manifest, crate::test_support::inline_coder_manifest()).unwrap();
        crate::config::with_isolated_config_path("spawn-warn-broken-config", |fake_dir| {
            std::fs::write(fake_dir.join("config.toml"), "not = valid = toml").unwrap();
            assert!(
                read_path_warning_for_spawn(&SpawnArgs {
                    blueprint_path: manifest.to_string_lossy().into_owned(),
                    ..SpawnArgs::default()
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
            let args = SpawnArgs {
                blueprint_path: blueprint_path.clone(),
                yolo: true,
                ..SpawnArgs::default()
            };
            let joined = held_checkpoint_warning_for_spawn(&args).join("\n");
            assert!(joined.contains("plan: plan_approval"), "{joined}");
            warn_held_checkpoints(&args);

            // An attended run stops for a person everywhere, so there is nothing
            // to announce.
            assert!(
                held_checkpoint_warning_for_spawn(&SpawnArgs {
                    blueprint_path: blueprint_path.clone(),
                    yolo: false,
                    ..SpawnArgs::default()
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
            held_checkpoint_warning_for_spawn(&SpawnArgs {
                blueprint_path: missing.to_string_lossy().into_owned(),
                yolo: true,
                ..SpawnArgs::default()
            })
            .is_empty()
        );

        let unparseable = dir.path().join("agent.leviath");
        std::fs::write(&unparseable, "not valid toml [[[").unwrap();
        assert!(
            held_checkpoint_warning_for_spawn(&SpawnArgs {
                blueprint_path: unparseable.to_string_lossy().into_owned(),
                yolo: true,
                ..SpawnArgs::default()
            })
            .is_empty()
        );

        // A config that will not load reads as no deadline, which is also the
        // default: the checkpoints still hold, and naming them matters more
        // than naming a timeout the operator may not have set.
        let held = manifest_with_a_held_checkpoint(dir.path());
        crate::config::with_isolated_config_path("spawn-held-broken-config", |fake_dir| {
            std::fs::write(fake_dir.join("config.toml"), "not = valid = toml").unwrap();
            let joined = held_checkpoint_warning_for_spawn(&SpawnArgs {
                blueprint_path: held.clone(),
                yolo: true,
                ..SpawnArgs::default()
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
        let args = SpawnArgs {
            blueprint_path: blueprint_path.clone(),
            output: format_request("json"),
            ..SpawnArgs::default()
        };
        let joined = retired_check_warning_for_spawn(&args).join("\n");
        assert!(joined.contains("checks/report.rhai"), "{joined}");
        assert!(joined.contains("stage 'plan'"), "{joined}");
        assert!(joined.contains("'json'"), "{joined}");
        warn_retired_output_checks(&args);

        assert!(
            retired_check_warning_for_spawn(&SpawnArgs {
                blueprint_path: blueprint_path.clone(),
                output: format_request("markdown"),
                ..SpawnArgs::default()
            })
            .is_empty(),
            "re-stating the declared format keeps the checks"
        );
        assert!(
            retired_check_warning_for_spawn(&SpawnArgs {
                blueprint_path,
                output: None,
                ..SpawnArgs::default()
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
            retired_check_warning_for_spawn(&SpawnArgs {
                blueprint_path: dir
                    .path()
                    .join("nope.leviath")
                    .to_string_lossy()
                    .into_owned(),
                output: format_request("json"),
                ..SpawnArgs::default()
            })
            .is_empty()
        );

        let unparseable = dir.path().join("agent.leviath");
        std::fs::write(&unparseable, "not valid toml [[[").unwrap();
        assert!(
            retired_check_warning_for_spawn(&SpawnArgs {
                blueprint_path: unparseable.to_string_lossy().into_owned(),
                output: format_request("json"),
                ..SpawnArgs::default()
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
    async fn send_spawn_reports_unexpected_response() {
        let err = send(r#"{"result":"ok","ok":true}"#).await.unwrap_err();
        assert!(err.to_string().contains("unexpected"));
    }

    #[tokio::test]
    async fn send_spawn_errors_when_daemon_absent() {
        let dir = tempfile::tempdir().unwrap();
        // A control id with no daemon bound to it.
        let id = control_id(&dir.path().join("no-daemon"));
        let err = send_spawn(&ControlClient::new(id), SpawnArgs::default(), false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not reachable"));
    }
}
