//! Blueprint resolution and spawn-request construction for an Agent Client
//! Protocol session.
//!
//! Pure helpers: turning a `session/new`'s working directory (and an optional
//! `--agent` name) into a resolved blueprint, and a resolved blueprint plus a
//! `session/prompt`'s task into the [`SpawnArgs`] the shared-world daemon
//! consumes.

use std::path::PathBuf;

use leviath_runtime::host::SpawnArgs;

use super::AgentClientArgs;
use crate::commands::run::manifest::find_manifest;
use crate::runstate::new_run_id;

/// A blueprint resolved for a session: its manifest file and the agent name
/// derived from the manifest's directory (matching `lev run`'s convention).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ResolvedBlueprint {
    /// Absolute path to the `agent.leviath` manifest.
    pub(super) manifest_path: PathBuf,
    /// The agent's name - its manifest directory's file name.
    pub(super) agent_name: String,
}

/// Resolve the blueprint a session should run.
///
/// When `--agent <name>` was given it wins, resolved through [`find_manifest`]
/// (which searches an explicit path, a directory, an installed agent by name,
/// then the process cwd). Otherwise the session's own `cwd` is searched for an
/// `agent.leviath`. A resolution failure is returned so the caller can answer
/// `session/new` with a JSON-RPC error rather than spawning nothing.
pub(super) fn resolve_blueprint(
    agent: Option<&str>,
    cwd: &str,
) -> anyhow::Result<ResolvedBlueprint> {
    let reference = agent.unwrap_or(cwd);
    let manifest_path = find_manifest(reference)?;
    let agent_name = manifest_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("agent")
        .to_string();
    Ok(ResolvedBlueprint {
        manifest_path,
        agent_name,
    })
}

/// Build the daemon spawn request for a resolved blueprint's first prompt.
///
/// `cwd` becomes the tool-execution working directory; `--yolo` / `--allow` /
/// `--max-depth` from the CLI flow through to the daemon's tool-policy
/// resolution. The model is left `None` so the blueprint's own per-stage model
/// selection stands. This is a top-level run, so `parent_run_id` is `None`.
pub(super) fn spawn_args(
    blueprint: &ResolvedBlueprint,
    task: &str,
    cwd: &str,
    args: &AgentClientArgs,
    regions: std::collections::HashMap<String, String>,
) -> SpawnArgs {
    SpawnArgs {
        run_id: new_run_id(&blueprint.agent_name),
        blueprint_path: blueprint.manifest_path.to_string_lossy().to_string(),
        task: task.to_string(),
        regions,
        model: None,
        workdir: cwd.to_string(),
        metadata: Default::default(),
        callback_url: None,
        callback_secret: None,
        yolo: args.yolo,
        no_seed_commands: args.no_seed_commands,
        allow: args.allow.clone(),
        max_depth: args.max_depth,
        parent_run_id: None,
        // A host that wants a particular shape says so when it starts the
        // server; ACP itself carries no field for it.
        output: match (&args.output_format, &args.output_instructions) {
            (None, None) => None,
            (format, instructions) => Some(leviath_core::output::OutputSpec {
                format: format.clone(),
                instructions: instructions.clone(),
                example: None,
                schema: None,
                validator: None,
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_blueprint(dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("agent.leviath"),
            format!(
                r#"
[agent]
name = "{name}"
version = "1.0.0"
description = "test blueprint"

[stages.plan]
prompt = "Plan the work"
"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn resolve_blueprint_from_a_cwd_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("coder");
        write_blueprint(&dir, "coder");
        let resolved = resolve_blueprint(None, &dir.to_string_lossy()).unwrap();
        assert_eq!(resolved.manifest_path, dir.join("agent.leviath"));
        // The agent name is the manifest directory's file name.
        assert_eq!(resolved.agent_name, "coder");
    }

    #[test]
    fn resolve_blueprint_prefers_an_explicit_agent_path() {
        let root = tempfile::tempdir().unwrap();
        let agent_dir = root.path().join("reviewer");
        write_blueprint(&agent_dir, "reviewer");
        // cwd points somewhere with no blueprint; --agent still resolves.
        let empty = tempfile::tempdir().unwrap();
        let resolved = resolve_blueprint(
            Some(&agent_dir.to_string_lossy()),
            &empty.path().to_string_lossy(),
        )
        .unwrap();
        assert_eq!(resolved.manifest_path, agent_dir.join("agent.leviath"));
        assert_eq!(resolved.agent_name, "reviewer");
    }

    #[test]
    fn resolve_blueprint_errors_when_nothing_is_found() {
        let empty = tempfile::tempdir().unwrap();
        assert!(resolve_blueprint(None, &empty.path().to_string_lossy()).is_err());
    }

    #[test]
    fn spawn_args_carry_cli_overrides_and_leave_model_default() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("coder");
        write_blueprint(&dir, "coder");
        let resolved = resolve_blueprint(None, &dir.to_string_lossy()).unwrap();
        let args = AgentClientArgs {
            agent: None,
            yolo: true,
            no_seed_commands: false,
            allow: vec!["bash".to_string()],
            max_depth: Some(2),
            output_format: None,
            output_instructions: None,
        };
        let regions =
            std::collections::HashMap::from([("criteria".to_string(), "be safe".to_string())]);
        let spawn = spawn_args(&resolved, "do the thing", "/work", &args, regions);
        assert_eq!(
            spawn.blueprint_path,
            resolved.manifest_path.to_string_lossy()
        );
        assert_eq!(spawn.task, "do the thing");
        assert_eq!(
            spawn.regions.get("criteria").map(String::as_str),
            Some("be safe")
        );
        assert_eq!(spawn.workdir, "/work");
        assert!(spawn.model.is_none());
        assert!(spawn.yolo);
        assert_eq!(spawn.allow, vec!["bash".to_string()]);
        assert_eq!(spawn.max_depth, Some(2));
        assert!(spawn.parent_run_id.is_none());
        // The run id is derived from the agent name.
        assert!(spawn.run_id.starts_with(&resolved.agent_name));
        // Nothing asked for a shape, so the blueprint's own stands.
        assert!(spawn.output.is_none());
    }

    /// ACP carries no field for an output shape, so a host that wants one says
    /// so when it starts the server. The label is passed through untouched.
    #[test]
    fn spawn_args_carry_a_requested_output_shape() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("coder");
        write_blueprint(&dir, "coder");
        let resolved = resolve_blueprint(None, &dir.to_string_lossy()).unwrap();
        let args = AgentClientArgs {
            agent: None,
            yolo: false,
            no_seed_commands: false,
            allow: vec![],
            max_depth: None,
            output_format: Some("a2ui".to_string()),
            output_instructions: Some("One card per finding.".to_string()),
        };
        let spawn = spawn_args(
            &resolved,
            "do the thing",
            "/work",
            &args,
            std::collections::HashMap::new(),
        );
        let spec = spawn.output.expect("the host asked for a shape");
        assert_eq!(spec.format.as_deref(), Some("a2ui"));
        assert_eq!(spec.instructions.as_deref(), Some("One card per finding."));
        // No schema from this path: a CLI flag is a poor place to compose one,
        // and the blueprint can declare it instead.
        assert!(spec.schema.is_none());
    }
}
