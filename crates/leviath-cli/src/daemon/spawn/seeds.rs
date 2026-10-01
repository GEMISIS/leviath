//! What an agent may read outside its workdir, and the files a seed reads.

use super::*;

/// Read each file and concatenate with `--- <path> ---` headers. Returns
/// `Ok(None)` when the list is empty; a missing or unreadable file is an
/// error naming it.
pub(crate) fn read_and_concat(
    region: &str,
    paths: impl Iterator<Item = std::path::PathBuf>,
) -> Result<Option<String>, String> {
    let mut parts: Vec<String> = Vec::new();
    for path in paths {
        match std::fs::read_to_string(&path) {
            // Held to the same size a script's I/O is: a seed lands in the
            // prompt whole, and a multi-megabyte file is not a seed.
            Ok(text) => parts.push(format!(
                "--- {} ---\n{}",
                path.display(),
                crate::daemon::script_host::cap_script_io(text)
            )),
            Err(e) => {
                return Err(format!(
                    "region '{region}': read seed file '{}': {e}",
                    path.display()
                ));
            }
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
}

/// Resolve an agent's `[read_paths]` declarations against the user's config
/// into the policy its file tools enforce, plus a warning to surface when the
/// declarations exist but nothing grants them.
///
/// A declared-but-ungranted agent still starts - its out-of-workdir reads are
/// refused per path with the same guidance - but the warning fires once here
/// so the user learns about it at the start rather than from a mid-run tool
/// error. A malformed entry (in the blueprint or in the user's own grant list)
/// is a hard error: silently dropping it would either under-grant or run the
/// agent with less vision than its author designed for.
///
/// Taken over its parts rather than a whole blueprint, so a run that has
/// already started can redo it: [`AgentToolState::reread_config`] keeps only
/// the declared half on the run.
///
/// [`AgentToolState::reread_config`]: crate::daemon::tool_service::AgentToolState::reread_config
pub(crate) fn compile_read_path_policy(
    agent_name: &str,
    declared: Option<&leviath_runtime::spec::blueprint::ReadPathsConfig>,
    config: &crate::config::Config,
    workdir: &std::path::Path,
) -> Result<(leviath_core::ReadPathPolicy, Option<String>), String> {
    let Some(rp) = declared.filter(|rp| !rp.allow.is_empty()) else {
        return Ok((leviath_core::ReadPathPolicy::inactive(), None));
    };
    let home = leviath_core::home_dir();
    let declared =
        leviath_core::ReadPathSet::compile(&rp.allow, workdir, home.as_deref(), cfg!(windows))
            .map_err(|e| format!("agent '{agent_name}' [read_paths]: {e}"))?;
    let grant_entries = config.read_path_grants_for_agent(agent_name);
    let grants =
        leviath_core::ReadPathSet::compile(&grant_entries, workdir, home.as_deref(), cfg!(windows))
            .map_err(|e| format!("read_paths grant in your config.toml: {e}"))?;
    let allow_blueprint = config.security.allow_blueprint_read_paths;
    let warning = (!allow_blueprint && grants.is_empty()).then(|| {
        let entries = rp
            .allow
            .iter()
            .map(|e| format!("\"{e}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "agent '{name}' declares [read_paths] but nothing grants them; reads outside \
             the workdir will be refused. To grant them, add to your config.toml either:\n\
             [security]\nallow_blueprint_read_paths = true\n\
             or the specific paths:\n[agent_read_paths.{name}]\nallow = [{entries}]",
            name = agent_name,
        )
    });
    Ok((
        leviath_core::ReadPathPolicy {
            agent: agent_name.to_string(),
            blueprint: declared,
            grants,
            allow_blueprint,
        },
        warning,
    ))
}

/// The policy a resuming run should enforce, or `None` when the entries no
/// longer compile.
///
/// A resume has nowhere to report a bad entry to: the run is already going and
/// the person is watching it, not a spawn. Refusing loudly at spawn and keeping
/// the working policy at resume is the same posture the config reloader takes
/// with a half-saved file, and it fails in the safe direction - a run keeps the
/// grants it had rather than losing them to a typo.
pub(crate) fn read_path_policy_for(
    agent_name: &str,
    declared: Option<&leviath_runtime::spec::blueprint::ReadPathsConfig>,
    config: &crate::config::Config,
    workdir: &std::path::Path,
) -> Option<leviath_core::ReadPathPolicy> {
    match compile_read_path_policy(agent_name, declared, config, workdir) {
        Ok((policy, _warning)) => Some(policy),
        Err(error) => {
            // Pre-bound rather than left as lazy `%` fields: a method call or a
            // borrow inside a structured field only runs when the callsite is
            // enabled, and tracing caches that interest process-wide, so under
            // a coverage run the region can be unreachable.
            let agent = agent_name;
            let reason = error;
            tracing::warn!(
                agent = %agent,
                error = %reason,
                "the [read_paths] in config.toml would not compile; the run keeps the ones it had"
            );
            None
        }
    }
}

/// Raise the read tools to `Private` for an agent whose `[read_paths]` are
/// actually granted: they can pull in content from outside the workdir -
/// design docs, run archives, whatever else was granted - which the default
/// `Internal` classification (written for workdir files) understates.
pub(crate) fn bump_read_sensitivities(
    map: &mut HashMap<String, leviath_core::TaintLevel>,
    read_paths_granted: bool,
) {
    if !read_paths_granted {
        return;
    }
    for tool in ["read_file", "read_files", "list_dir"] {
        if let Some(level) = map.get_mut(tool) {
            *level = (*level).max(leviath_core::TaintLevel::Private);
        }
    }
}
