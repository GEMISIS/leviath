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
use crate::commands::run::session::read_region_value;
use crate::runstate::new_run_id;

/// Resolve the local inputs of a spawn request: find the manifest, mint a run id
/// from the agent's directory name, record the working directory, and resolve
/// any dynamic `--<region>` flags (raw values, `@path` or literal) against the
/// blueprint's declared caller-input regions.
///
/// `regions` maps a flag name to its raw value. An unknown region name (one the
/// blueprint doesn't read as caller input) is a hard error here — fast, local
/// typo protection before the daemon is contacted.
#[allow(clippy::too_many_arguments)]
pub fn resolve_spawn_args(
    path: &str,
    task: &str,
    model: Option<String>,
    workdir: &str,
    yolo: bool,
    allow: Vec<String>,
    max_depth: Option<usize>,
    regions: HashMap<String, String>,
    no_seed_commands: bool,
) -> anyhow::Result<SpawnArgs> {
    let manifest = find_manifest(path)?;
    let agent_name = manifest
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("agent");

    // Validate + resolve region flags against the blueprint's caller-input regions.
    let resolved_regions = if regions.is_empty() {
        HashMap::new()
    } else {
        let content = std::fs::read_to_string(&manifest)
            .map_err(|e| anyhow::anyhow!("read manifest '{}': {e}", manifest.display()))?;
        let blueprint = leviath_core::manifest::parse_manifest(&content)
            .map_err(|e| anyhow::anyhow!("parse manifest: {e}"))?;
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
        out
    };

    Ok(SpawnArgs {
        run_id: new_run_id(agent_name),
        blueprint_path: manifest.to_string_lossy().to_string(),
        task: task.to_string(),
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

/// Send a resolved spawn request to the daemon and report the outcome, printing
/// the new run id on success.
pub async fn send_spawn(client: &ControlClient, spawn_args: SpawnArgs) -> anyhow::Result<()> {
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
            "do it",
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
        assert_eq!(args.blueprint_path, manifest.to_string_lossy());
        assert_eq!(args.workdir, "/work");
    }

    #[test]
    fn resolve_spawn_args_errors_on_missing_manifest() {
        assert!(
            resolve_spawn_args(
                "/no/such/agent",
                "t",
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
            "review it",
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
            "t",
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
            "t",
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
            "t",
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
            "review it",
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
            "review it",
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
            let stream = listener.accept().await.unwrap();
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
