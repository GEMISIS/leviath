//! Client-side helpers for talking to the shared-world daemon: sending a
//! spawn request `lev run` built ([`crate::commands::run::request`]) and
//! reporting what the daemon made of it, with the warnings a person should see
//! before the run starts. The socket-path resolution + connect live in the
//! binary; these cores are unit-testable against a fake socket server.

use anyhow::bail;
use leviath_runtime::control_socket::{ControlClient, ControlResponse};
use leviath_runtime::spec::request::SpawnRequest;

pub use crate::commands::run::request::{LaunchRequest, resolve_spawn_args};

/// The stdin probe for a caller that must never open an editor for a task:
/// the dashboard, which owns the terminal itself. An editor launched under it
/// would fight it for the screen.
///
/// Passing this rather than a bare `|| false` states the reason at each call
/// site.
pub(crate) fn never_interactive() -> bool {
    false
}

/// A `lev run` command line, resolved locally: the request to send, and what
/// the warnings before it and the report after it read.
#[derive(Debug, Clone)]
pub struct LocalRun {
    /// The request.
    pub request: SpawnRequest,
    /// The blueprint file the request names. Empty for a raw graph, which has none.
    pub manifest: std::path::PathBuf,
    /// The working directory, as given.
    pub workdir: String,
    /// Whether the run is unattended.
    pub yolo: bool,
    /// The yolo profile, when one was named.
    pub yolo_profile: Option<String>,
    /// The output shape asked for, for the warning about retired checks.
    pub output: Option<leviath_core::output::OutputSpec>,
    /// `--check`: ask the daemon what the run would be instead of starting it.
    pub check: bool,
    /// What the command line itself found wrong. A run with any is never
    /// spawned: the daemon is asked only to check the rest, and every problem
    /// is reported together.
    pub issues: leviath_runtime::spec::issues::SpawnIssues,
    /// Whether the run's task was left unasked because of those problems.
    pub task_unasked: bool,
}

/// Warn, on stderr, when the agent about to run declares `[read_paths]` the
/// active config does not grant.
///
/// The daemon already logs this at spawn, but into its own log, where the
/// person who just typed `lev run` never sees it - so the first sign of a
/// missing grant was a refused read partway through a run. Everything needed to
/// say it here is local: `lev run` resolves the blueprint itself, and the config
/// is the same file the daemon reads.
///
/// Best-effort by design. An unreadable blueprint or config is the daemon's to
/// report, and it will: this must never be the reason a run does not start.
fn warn_ungranted_read_paths(run: &LocalRun) {
    for line in read_path_warning_for_spawn(run) {
        eprintln!("{line}");
    }
}

/// The warning for a spawn request, read from the real blueprint and config.
/// Empty when there is nothing to say, and empty when either file cannot be
/// read: see [`warn_ungranted_read_paths`] for why that is not an error here.
fn read_path_warning_for_spawn(run: &LocalRun) -> Vec<String> {
    let Some(loaded) = crate::commands::run::locate::loaded_at(&run.manifest) else {
        return Vec::new();
    };
    let Ok(config) = crate::config::Config::load() else {
        return Vec::new();
    };
    spawn_warning_lines(
        &loaded.graph,
        loaded.reference.name.as_str(),
        &config,
        std::path::Path::new(&run.workdir),
    )
}

/// The warning itself: one line saying what is refused, then the stanza that
/// would grant it. Pure, so the wording is testable without a daemon.
fn spawn_warning_lines(
    graph: &leviath_runtime::spec::graph::RunGraph,
    agent: &str,
    config: &crate::config::Config,
    workdir: &std::path::Path,
) -> Vec<String> {
    let Some(Ok(report)) = crate::read_path_report::build(graph, agent, config, workdir) else {
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
/// unreadable blueprint or config is the daemon's to report, and this must never
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
    let Some(loaded) = crate::commands::run::locate::loaded_at(path) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = crate::bundled::stale_install_note(
        path,
        loaded.reference.name.as_str(),
        &loaded.version,
        leviath_core::agents_dir().as_deref(),
    )
    .into_iter()
    .collect();
    if run.yolo {
        let timeout = crate::config::Config::load()
            .ok()
            .and_then(|c| c.limits.interaction_timeout_secs);
        lines.extend(crate::held_checkpoints::preflight_lines(
            &loaded.graph,
            timeout,
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

/// The retirement warning for a spawn request, read from the real blueprint.
/// Best-effort for the same reason as [`warn_ungranted_read_paths`].
fn retired_check_warning_for_spawn(run: &LocalRun) -> Vec<String> {
    match (
        run.output.as_ref(),
        crate::commands::run::locate::loaded_at(&run.manifest),
    ) {
        (Some(request), Some(loaded)) => {
            retired_checks::retired_check_warnings(&loaded.graph, Some(request))
        }
        _ => Vec::new(),
    }
}

mod retired_checks;

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
    /// The blueprint file the run was resolved from.
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
/// already started - those runs keep running; `lev ps` lists them. A `--check`
/// run starts nothing: it is sent to be checked, once.
pub async fn send_spawn_batch(
    client: &ControlClient,
    run: LocalRun,
    count: usize,
    json: bool,
) -> anyhow::Result<()> {
    if run.check {
        return crate::commands::run::check::send_check(client, &run, json).await;
    }
    if count == 0 {
        bail!("--count must be at least 1");
    }
    // The command line's own problems refuse the run; the daemon only checks
    // the rest, so they are all reported at once.
    if !run.issues.is_empty() {
        return crate::commands::run::check::send_check(client, &run, json).await;
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
        Ok(ControlResponse::Rejected { issues }) => bail!(
            "the daemon refused the run.\n{}",
            crate::commands::run::request::issues_report(&issues)
        ),
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
            check: false,
            issues: Default::default(),
            task_unasked: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::control_socket::{ControlId, bind_control_listener, control_id};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::task::JoinHandle;
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

    /// A `--check` run is sent to be checked, never spawned, whatever the
    /// count: the daemon's answer is the check's.
    #[tokio::test]
    async fn a_check_run_is_checked_not_spawned() {
        let dir = tempfile::tempdir().unwrap();
        let (id, server) = fake_daemon(dir.path(), r#"{"result":"error","message":"boom"}"#);
        let run = LocalRun {
            check: true,
            ..LocalRun::default()
        };
        let err = send_spawn_batch(&ControlClient::new(id), run, 3, false)
            .await
            .expect_err("the daemon refused");
        server.await.unwrap();
        assert!(err.to_string().contains("the check failed: boom"), "{err}");
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
            blueprint_path: "/agents/coder/agent.toml".to_string(),
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
    fn read_paths_blueprint() -> leviath_runtime::spec::graph::RunGraph {
        leviath_blueprint::BlueprintFile::parse(
            r#"
[blueprint]
name = "cto"
version = "0.1.0"
description = "test"

[graph]
read_paths = ["/data/runs"]
stages = [{ name = "main" }]
layout = { total_budget_tokens = 1000, regions = [{ name = "system", kind = "pinned", budget = 1000 }] }
"#,
        )
        .expect("blueprint parses")
        .run_graph()
    }

    /// The point of warning here at all: the person who typed `lev run` learns
    /// the declaration is inert now, not at the first refused read.
    #[test]
    fn an_ungranted_declaration_warns_with_the_stanza_to_add() {
        let lines = spawn_warning_lines(
            &read_paths_blueprint(),
            "cto",
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
                "cto",
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
            leviath_blueprint::BlueprintFile::parse(&crate::test_support::inline_coder_manifest())
                .expect("blueprint parses")
                .run_graph();
        assert!(
            spawn_warning_lines(
                &plain,
                "coder",
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
                "cto",
                &broken,
                std::path::Path::new("/work")
            )
            .is_empty()
        );
    }

    /// The coder blueprint, declaring `read_paths`, written to `dir`.
    fn coder_reading(dir: &std::path::Path, read_paths: &[&str]) -> std::path::PathBuf {
        let mut file =
            leviath_blueprint::BlueprintFile::parse(&crate::test_support::inline_coder_manifest())
                .expect("blueprint parses");
        file.graph.read_paths = read_paths.iter().map(|p| p.to_string()).collect();
        let path = dir.join(leviath_blueprint::FILE_NAME);
        std::fs::write(&path, file.to_toml().expect("it writes")).unwrap();
        path
    }

    /// End to end over the real files: a blueprint on disk plus an isolated
    /// config that grants nothing.
    #[tokio::test]
    async fn the_warning_reads_the_blueprint_and_the_active_config() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = coder_reading(dir.path(), &["/data/runs"]);
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

    /// Every way the warning can decline to run: a blueprint that will not
    /// parse, and a config that will not load. Neither may stop a spawn.
    #[test]
    fn the_warning_gives_up_quietly_on_a_broken_blueprint_or_config() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join(leviath_blueprint::FILE_NAME);
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

    /// A blueprint that declares a held checkpoint, written to disk, so the
    /// warning is exercised through the real read-and-parse path.
    fn manifest_with_a_held_checkpoint(dir: &std::path::Path) -> String {
        let manifest = dir.join(leviath_blueprint::FILE_NAME);
        std::fs::write(
            &manifest,
            r#"
[blueprint]
name = "held"
version = "0.1.0"
description = "holds a checkpoint"

[graph]
entry = "plan"

[[graph.stages]]
name = "plan"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
tools = ["read_file"]
max_iterations = 5

[[graph.stages.mode.interactive_points]]
name = "plan_approval"
prompt = "Review the plan"
style = "confirm"
unattended = "ask"

[graph.layout]
total_budget_tokens = 11000
regions = [
    { name = "system", kind = "pinned", budget = 1000 },
    { name = "conversation", kind = { kind = "sliding_window", max_items = 50 }, budget = 10000 },
]
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

    /// The same three lenient arms as the read-path warning: a blueprint that
    /// is not there, one that will not parse, and a config that will not load.
    /// None of them may stop a spawn.
    #[test]
    fn the_held_checkpoint_warning_gives_up_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        assert!(
            held_checkpoint_warning_for_spawn(&LocalRun {
                manifest: std::path::PathBuf::from(missing.to_string_lossy().into_owned()),
                yolo: true,
                ..LocalRun::default()
            })
            .is_empty()
        );

        let unparseable = dir.path().join("other").join(leviath_blueprint::FILE_NAME);
        std::fs::create_dir_all(unparseable.parent().unwrap()).unwrap();
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

    /// A blueprint declaring a Rhai validator for its output, written to disk,
    /// so the retirement warning is exercised through the real read-and-parse
    /// path.
    fn manifest_with_a_validator(dir: &std::path::Path) -> String {
        let manifest = dir.join(leviath_blueprint::FILE_NAME);
        std::fs::write(
            &manifest,
            r#"
[blueprint]
name = "checked"
version = "0.1.0"
description = "declares a validator"

[graph]
output = { format = "markdown", validator = { file = "checks/report.rhai" } }
layout = { total_budget_tokens = 1000, regions = [{ name = "system", kind = "pinned", budget = 1000 }] }

[[graph.stages]]
name = "plan"
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

    /// The same lenient arms as every warning on this path: a blueprint that
    /// is not there or will not parse must stay quiet, never stop a spawn.
    #[test]
    fn the_retirement_warning_gives_up_quietly() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            retired_check_warning_for_spawn(&LocalRun {
                manifest: dir.path().join("nope.toml"),
                output: format_request("json"),
                ..LocalRun::default()
            })
            .is_empty()
        );

        let unparseable = dir.path().join(leviath_blueprint::FILE_NAME);
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
