//! Client-side helpers for talking to the shared-world daemon: building a spawn
//! request from local inputs and exchanging it over the control socket. Shared by
//! `lev run` (and reusable by other clients). The socket-path resolution + connect
//! live in the binary; these cores are unit-testable against a fake socket server.

use std::collections::HashMap;

use anyhow::bail;
use leviath_core::layout::RegionSeed;
use leviath_runtime::control_socket::{ControlClient, ControlResponse};
use leviath_runtime::host::SpawnArgs;

use crate::commands::run::manifest::find_manifest;
use crate::commands::run::task::{read_region_value, resolve_task};
use crate::runstate::new_run_id;

/// Everything a spawn request needs from the agent's own files.
pub struct AgentSource {
    /// The resolved `agent.leviath` path.
    pub manifest: std::path::PathBuf,
    /// The manifest's parent directory name, which the run id is minted from.
    /// Deliberately not `blueprint.name`: the run id is what `lev ps` shows and
    /// what identifies the checkout on disk, while the blueprint's own name is
    /// what the agent calls itself.
    pub run_stem: String,
    pub blueprint: leviath_core::Blueprint,
}

/// Find the agent's manifest and parse it, once.
///
/// The parse is unconditional. It used to happen only when there were region
/// flags to validate, but the blueprint's name and description are now needed
/// for the editor template too, and parsing here is strictly better regardless:
/// it is the same parser the daemon runs on the same file moments later, so a
/// manifest that fails here would have failed there, and `parse manifest: <toml
/// error>` before the daemon is contacted beats a spawn rejection after.
pub fn load_agent_source(path: &str) -> anyhow::Result<AgentSource> {
    let found = find_manifest(path)?;
    // Absolute, because this path is about to be handed to the daemon, which
    // has its own working directory. `lev run .` and `lev run ./demo` resolve
    // fine here and then arrive there as `./agent.leviath`, which the daemon
    // reads relative to wherever it happens to have been started - so the spawn
    // failed with "read manifest './agent.leviath': No such file or directory".
    // `lev create` prints `lev run .` as its next step, so this was the first
    // thing a new user hit.
    //
    // Best-effort rather than fallible: `find_manifest` only returns paths it
    // has already confirmed resolve, so a failure here needs the file to vanish
    // between the two calls. Falling back to what it found leaves the old
    // behavior, which is a legible daemon-side error, rather than inventing an
    // error arm no test can reach.
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
    let declared: Vec<String> = blueprint
        .context_layout
        .regions
        .iter()
        .filter_map(|r| match &r.seed {
            Some(RegionSeed::CallerInput { name }) => Some(name.clone()),
            _ => None,
        })
        .collect();
    let mut out = HashMap::new();
    for (name, raw) in regions {
        if !declared.contains(&name) {
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
pub fn never_interactive() -> bool {
    false
}

/// Resolve the local inputs of a spawn request: find and parse the manifest,
/// resolve the `--<region>` flags, resolve the task, and mint a run id from the
/// agent's directory name.
///
/// `task` is what `--task` was given, if anything. Left off, [`resolve_task`]
/// opens the user's editor, which is why `stdin_is_terminal` is threaded
/// through: the probe itself is real I/O and belongs to the binary, so callers
/// inject it (tests pass a `fn` that always says no).
///
/// Regions are resolved *before* the task on purpose. A typo'd `--foo` has to
/// fail before the user is dropped into an editor and types a paragraph they
/// are about to lose.
#[allow(clippy::too_many_arguments)]
pub fn resolve_spawn_args(
    path: &str,
    task: Option<&str>,
    stdin_is_terminal: &dyn Fn() -> bool,
    model: Option<String>,
    workdir: &str,
    yolo: bool,
    allow: Vec<String>,
    max_depth: Option<usize>,
    regions: HashMap<String, String>,
    no_seed_commands: bool,
) -> anyhow::Result<SpawnArgs> {
    let source = load_agent_source(path)?;
    let resolved_regions = resolve_regions(&source.blueprint, regions)?;
    let task = resolve_task(
        task,
        &source.blueprint.name,
        &source.blueprint.description,
        stdin_is_terminal,
    )?;

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
fn read_path_warning_for_spawn(spawn_args: &SpawnArgs) -> Vec<String> {
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

/// Send a resolved spawn request to the daemon and report the outcome, printing
/// the new run id on success.
pub async fn send_spawn(client: &ControlClient, spawn_args: SpawnArgs) -> anyhow::Result<()> {
    warn_ungranted_read_paths(&spawn_args);
    match client.spawn(spawn_args).await {
        Ok(ControlResponse::Spawned { run_id }) => {
            println!("spawned {run_id}");
            Ok(())
        }
        Ok(ControlResponse::Error { message }) => bail!("spawn failed: {message}"),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
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

        let args = resolve_spawn_args(
            manifest.to_str().unwrap(),
            Some("do it"),
            &never_interactive,
            Some("m".to_string()),
            "/work",
            false,
            Vec::new(),
            None,
            HashMap::new(),
            false,
        )
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
    /// resolved before the request leaves. `lev run .` used to reach the daemon
    /// as `./agent.leviath` and fail there, which is the very command
    /// `lev create` prints as the next step.
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

        let args = resolve_spawn_args(
            relative.to_str().unwrap(),
            Some("do it"),
            &never_interactive,
            None,
            "/work",
            false,
            Vec::new(),
            None,
            HashMap::new(),
            false,
        )
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
            resolve_spawn_args(
                "/no/such/agent",
                Some("t"),
                &never_interactive,
                None,
                "/work",
                false,
                Vec::new(),
                None,
                HashMap::new(),
                false,
            )
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

        let args = resolve_spawn_args(
            manifest.to_str().unwrap(),
            Some(task_file.to_str().unwrap()),
            &never_interactive,
            None,
            "/work",
            false,
            Vec::new(),
            None,
            HashMap::new(),
            false,
        )
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

        let err = resolve_spawn_args(
            manifest.to_str().unwrap(),
            None,
            &never_interactive,
            None,
            "/work",
            false,
            Vec::new(),
            None,
            HashMap::new(),
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("No task provided"), "got: {err}");
    }

    /// Pins the ordering: a typo'd region flag must fail *before* the user is
    /// dropped into an editor, or they type a paragraph and then lose it.
    #[test]
    fn resolve_spawn_args_rejects_a_bad_region_before_it_looks_at_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_region_manifest(&dir.path().join("reviewer"));
        let regions = HashMap::from([("bogus".to_string(), "x".to_string())]);

        let err = resolve_spawn_args(
            manifest.to_str().unwrap(),
            None,
            &never_interactive,
            None,
            "/work",
            false,
            Vec::new(),
            None,
            regions,
            false,
        )
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
        let args = resolve_spawn_args(
            manifest.to_str().unwrap(),
            Some("review it"),
            &never_interactive,
            None,
            "/work",
            false,
            Vec::new(),
            None,
            regions,
            false,
        )
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
        let err = resolve_spawn_args(
            manifest.to_str().unwrap(),
            Some("t"),
            &never_interactive,
            None,
            "/work",
            false,
            Vec::new(),
            None,
            regions,
            false,
        )
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
        let err = resolve_spawn_args(
            agent_dir.to_str().unwrap(),
            Some("t"),
            &never_interactive,
            None,
            "/work",
            false,
            Vec::new(),
            None,
            regions,
            false,
        )
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
        let err = resolve_spawn_args(
            agent_dir.join("agent.leviath").to_str().unwrap(),
            Some("t"),
            &never_interactive,
            None,
            "/work",
            false,
            Vec::new(),
            None,
            regions,
            false,
        )
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
        let err = resolve_spawn_args(
            manifest.to_str().unwrap(),
            Some("review it"),
            &never_interactive,
            None,
            "/work",
            false,
            Vec::new(),
            None,
            regions,
            false,
        )
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
        let err = resolve_spawn_args(
            manifest.to_str().unwrap(),
            Some("review it"),
            &never_interactive,
            None,
            "/work",
            false,
            Vec::new(),
            None,
            regions,
            false,
        )
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
        let result = send_spawn(&ControlClient::new(id), SpawnArgs::default()).await;
        server.await.unwrap();
        result
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
        let err = send_spawn(&ControlClient::new(id), SpawnArgs::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not reachable"));
    }
}
