//! What an agent starts with on disk, and what it may read afterwards.
//!
//! Seeds and `[read_paths]` are one concern wearing two names: both take a path
//! a *blueprint* chose and decide whether this run may read it. A seed does it
//! once at spawn, before any approval prompt exists; a read-path grant does it
//! on every later tool call. Both fence against the same escape, so the fence
//! is written once and used twice.

use super::*;

/// Resolve a blueprint-declared seed path against the run's working directory.
///
/// The same rule the `read_file` tool follows, and for the same reason: the
/// *blueprint* chose this path, not the user, so an installed package could
/// otherwise write `seed = { files = ["../../.leviath/config.toml"] }` and have
/// the provider keys in that file seeded straight into a pinned context region -
/// from where they travel to the model, and out through the answer, a webhook,
/// or a sub-agent. `read_file` has been confined for exactly this since the
/// symlink-escape fix; seeding was the path that stayed open.
///
/// Outside the workdir falls back to `[read_paths]`, which is already the
/// mechanism for "this agent is meant to read there and the user agreed",
/// rather than a second answer to the same question.
pub(super) fn seed_path_within(
    base: &std::path::Path,
    declared: &std::path::Path,
    read_paths: &leviath_core::ReadPathPolicy,
) -> Result<std::path::PathBuf, String> {
    if leviath_core::resolves_within(declared, base) {
        return Ok(declared.to_path_buf());
    }
    let refusal = || {
        format!(
            "seed path '{}' resolves outside the working directory ({}); grant it with \
             [read_paths] in the blueprint and your config, or move it inside",
            declared.display(),
            base.display()
        )
    };
    if !read_paths.is_active() {
        return Err(refusal());
    }
    // One arm for both ways this fails, because they fail the same way: a path
    // that cannot be canonicalized is never matched, exactly as a canonical one
    // the policy declines is not.
    leviath_core::canonicalize_for_match(declared)
        .filter(|c| {
            matches!(
                read_paths.decide(c),
                leviath_core::ReadPathDecision::Allowed
            )
        })
        .ok_or_else(refusal)
}

/// Resolve every region's initial content from its blueprint-declared
/// [`RegionSeed`] plus the caller-provided values on `args`, into a
/// name→content map ready for [`spawn_agent_seeded`].
///
/// The caller map is `{ "task": args.task } ∪ args.regions` (a `regions["task"]`
/// wins). Then:
/// - `CallerInput { name }` pulls from the caller map; if the region is
///   `required` and the value is missing/blank this returns `Err` - the
///   required-at-spawn gate, before any inference.
/// - `Files` / `Glob` read workdir files; `Literal` is verbatim; `Rhai` runs a
///   workdir script whose `String` return seeds the region.
/// - `Command` runs a shell command in the workdir under `commands` -
///   sandboxed, time- and size-capped, and skippable. Every failure is
///   non-fatal unless the region is `required`.
/// - Any caller key (other than `task`) that isn't a declared `CallerInput`
///   region is rejected (typo protection, mirrors the CLI-side check).
pub(super) fn resolve_seeds(
    blueprint: &Blueprint,
    args: &SpawnArgs,
    workdir: &str,
    commands: &SeedCommandPolicy,
    read_paths: &leviath_core::ReadPathPolicy,
) -> Result<HashMap<String, String>, String> {
    use leviath_core::layout::RegionSeed;

    // The effective caller-supplied values: task text plus any named regions.
    let mut caller: HashMap<String, String> = HashMap::new();
    caller.insert("task".to_string(), args.task.clone());
    for (k, v) in &args.regions {
        caller.insert(k.clone(), v.clone());
    }

    // Unknown caller keys are tolerated here (silently unused): the CLI already
    // rejects typos client-side in `resolve_spawn_args`, and an ACP host sending
    // a stray `---region:...---` marker shouldn't fail the whole turn over it.
    //
    // `task` is the exception, because it is not a stray marker - it is the
    // request. A blueprint that seeds no region from it drops the task on the
    // floor and runs anyway: the agent answers a question nobody asked, having
    // spent the tokens to do it. Four different models were observed replying
    // "I'm ready, what would you like?" to a task that had been supplied.
    // Refusing here costs one clear error instead of a plausible-looking run.
    // The CLI refuses this earlier and with the same message, but the API and
    // ACP paths do not go through the CLI at all, so the check lives here too.
    if !args.task.trim().is_empty() && !blueprint.accepts_task() {
        return Err(blueprint.task_refusal());
    }

    let base = std::path::Path::new(workdir);
    let mut seeds: HashMap<String, String> = HashMap::new();

    for region in &blueprint.context_layout.regions {
        let Some(seed) = &region.seed else { continue };
        match seed {
            RegionSeed::CallerInput { name } => {
                let value = caller.get(name).map(|s| s.as_str()).unwrap_or("");
                if value.trim().is_empty() {
                    if region.required {
                        return Err(region.required_message.clone().unwrap_or_else(|| {
                            format!(
                                "required region '{}' was not provided; supply it via \
                                 --{name} <text|@file> (CLI), a ---region:{name}--- block \
                                 (ACP), or the API `regions` field",
                                region.name
                            )
                        }));
                    }
                    // Optional and unprovided - leave the region empty.
                    continue;
                }
                seeds.insert(region.name.clone(), value.to_string());
            }
            RegionSeed::Literal { text } => {
                seeds.insert(region.name.clone(), text.clone());
            }
            RegionSeed::Files { paths } => {
                let resolved = paths
                    .iter()
                    .map(|p| seed_path_within(base, &base.join(p), read_paths))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("region '{}': {e}", region.name))?;
                let content = read_and_concat(&region.name, resolved.into_iter(), region.required)?;
                if let Some(content) = content {
                    seeds.insert(region.name.clone(), content);
                }
            }
            RegionSeed::Glob { pattern } => {
                let full = base.join(pattern);
                let full = full.to_string_lossy();
                let matches = glob::glob(&full)
                    .map_err(|e| format!("region '{}': bad glob '{pattern}': {e}", region.name))?;
                // Each *match* is checked, not the pattern: `../../*.toml`
                // cannot be judged before it is expanded.
                let paths = matches
                    .filter_map(|m| m.ok())
                    .map(|p| seed_path_within(base, &p, read_paths))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("region '{}': {e}", region.name))?;
                let content = read_and_concat(&region.name, paths.into_iter(), region.required)?;
                match content {
                    Some(content) => {
                        seeds.insert(region.name.clone(), content);
                    }
                    None if region.required => {
                        return Err(format!(
                            "required region '{}': glob '{pattern}' matched no files",
                            region.name
                        ));
                    }
                    None => {}
                }
            }
            RegionSeed::Rhai { script } => {
                let path = seed_path_within(base, &base.join(script), read_paths)
                    .map_err(|e| format!("region '{}': {e}", region.name))?;
                let src = std::fs::read_to_string(&path).map_err(|e| {
                    format!(
                        "region '{}': read rhai seed '{}': {e}",
                        region.name,
                        path.display()
                    )
                })?;
                let mut input = rhai::Map::new();
                input.insert("task".into(), rhai::Dynamic::from(args.task.clone()));
                input.insert("workdir".into(), rhai::Dynamic::from(workdir.to_string()));
                let out = leviath_scripting::ScriptEngine::new()
                    .transform(&src, input)
                    .map_err(|e| format!("region '{}': rhai seed failed: {e}", region.name))?;
                if !out.trim().is_empty() {
                    seeds.insert(region.name.clone(), out);
                } else if region.required {
                    return Err(format!(
                        "required region '{}': rhai seed '{script}' returned empty",
                        region.name
                    ));
                }
            }
            // A command seed *executes* at spawn, before any inference and so
            // before any tool-approval prompt. It is therefore skipped outright
            // when disabled, and every failure mode is non-fatal unless the
            // region is `required` (mirroring the Files/Glob arms above): a
            // discovery nicety must never be able to sink a run.
            RegionSeed::Command { command } => {
                if !commands.allowed {
                    if region.required {
                        return Err(format!(
                            "required region '{}': command seeds are disabled \
                             (`[security] allow_seed_commands = false` or --no-seed-commands)",
                            region.name
                        ));
                    }
                    tracing::warn!(
                        region = %region.name,
                        "command seed skipped: command seeds are disabled"
                    );
                    continue;
                }
                match commands.run(command, base) {
                    Ok(out) if !out.trim().is_empty() => {
                        seeds.insert(region.name.clone(), out);
                    }
                    Ok(_) => {
                        if region.required {
                            return Err(format!(
                                "required region '{}': command seed '{command}' returned empty",
                                region.name
                            ));
                        }
                        tracing::warn!(
                            region = %region.name,
                            command = %command,
                            "command seed returned no output; region left empty"
                        );
                    }
                    Err(e) => {
                        if region.required {
                            return Err(format!(
                                "required region '{}': command seed '{command}' failed: {e}",
                                region.name
                            ));
                        }
                        tracing::warn!(
                            region = %region.name,
                            command = %command,
                            error = %e,
                            "command seed failed; region left empty"
                        );
                    }
                }
            }
        }
    }

    Ok(seeds)
}

/// Read each file and concatenate with `--- <path> ---` headers. Returns
/// `Ok(None)` when the list is empty; a missing/unreadable file is an error only
/// when `required`, else it is skipped.
pub(super) fn read_and_concat(
    region: &str,
    paths: impl Iterator<Item = std::path::PathBuf>,
    required: bool,
) -> Result<Option<String>, String> {
    let mut parts: Vec<String> = Vec::new();
    for path in paths {
        match std::fs::read_to_string(&path) {
            Ok(text) => parts.push(format!("--- {} ---\n{}", path.display(), text)),
            Err(e) => {
                if required {
                    return Err(format!(
                        "region '{region}': read seed file '{}': {e}",
                        path.display()
                    ));
                }
            }
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
}

/// Resolve an agent's `[read_paths]` declarations against the user's config
/// into the policy its file tools enforce, plus a warning to surface when the
/// declarations exist but nothing grants them.
///
/// A declared-but-ungranted agent still spawns - its out-of-workdir reads are
/// refused per path with the same guidance - but the warning fires once here
/// so the user learns about it at spawn rather than from a mid-run tool error.
/// A malformed entry (in the blueprint or in the user's own grant list) is a
/// hard spawn error: silently dropping it would either under-grant or run the
/// agent with less vision than its author designed for.
pub(super) fn build_read_path_policy(
    blueprint: &leviath_core::Blueprint,
    config: &crate::config::Config,
    workdir: &std::path::Path,
) -> Result<(leviath_core::ReadPathPolicy, Option<String>), String> {
    let Some(rp) = blueprint
        .read_paths
        .as_ref()
        .filter(|rp| !rp.allow.is_empty())
    else {
        return Ok((leviath_core::ReadPathPolicy::inactive(), None));
    };
    let home = leviath_core::home_dir();
    let declared =
        leviath_core::ReadPathSet::compile(&rp.allow, workdir, home.as_deref(), cfg!(windows))
            .map_err(|e| format!("agent '{}' [read_paths]: {e}", blueprint.name))?;
    let grant_entries = config.read_path_grants_for_agent(&blueprint.name);
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
            name = blueprint.name,
        )
    });
    Ok((
        leviath_core::ReadPathPolicy {
            agent: blueprint.name.clone(),
            blueprint: declared,
            grants,
            allow_blueprint,
        },
        warning,
    ))
}

/// How many of a blueprint's `[read_paths]` entries the config grants, for the
/// run listing. `None` when the blueprint declares none, and when the user's
/// own grant list will not compile - that is a hard spawn error a line above,
/// so there is no half-answer to record.
pub(super) fn read_path_grant_counts(
    blueprint: &leviath_core::Blueprint,
    config: &crate::config::Config,
    workdir: &std::path::Path,
) -> Option<leviath_core::run_meta::ReadPathGrantCounts> {
    let report = crate::read_path_report::build(blueprint, config, workdir)?.ok()?;
    Some(leviath_core::run_meta::ReadPathGrantCounts {
        declared: report.declared(),
        granted: report.granted(),
    })
}

/// Raise the read tools to `Private` for an agent whose `[read_paths]` are
/// actually granted: they can pull in content from outside the workdir -
/// design docs, run archives, whatever else was granted - which the default
/// `Internal` classification (written for workdir files) understates.
pub(super) fn bump_read_sensitivities(
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
