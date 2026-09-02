//! What scripts a blueprint brings, and whether they may run here.
//!
//! Rhai tools, region hooks, stage hooks and output validators are all declared
//! by path in the manifest, so every one of them is a file the blueprint author
//! chose and this daemon is about to execute. Resolution and containment
//! therefore live together: `script_within_blueprint` is the fence, and nothing
//! below it loads a path that has not been through it.
//!
//! Platform capabilities are here for the same reason - a tool that declares a
//! capability the host does not provide is not advertised at all, which is a
//! decision about what runs rather than about how.

use super::*;

/// The directories scanned for an agent's Rhai script tools, in precedence order
/// (earlier wins on a name collision): the agent's own `<agent_dir>/tools/`, then
/// `extra` (the run workdir's `tools/`, only for `dynamic_tools` agents so a
/// mid-run write is picked up), then the global `~/.leviath/tools/`. `Option`'s
/// iterator flattens the "no parent" / "no home" cases without a dangling
/// `if let` else region.
pub(super) fn script_scan_dirs(
    blueprint_path: &str,
    extra: Option<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    std::path::Path::new(blueprint_path)
        .parent()
        .map(|d| d.join("tools"))
        .into_iter()
        .chain(extra)
        .chain(leviath_core::tools_dir())
        .collect()
}

/// Read and compile every custom region's Rhai script declared by `blueprint`
/// (global layout plus each stage's per-stage layout), keyed by the script
/// path as written. Paths resolve relative to the blueprint's directory (the
/// script-tool convention - the script travels with the agent), with absolute
/// paths passing through `Path::join` unchanged. Each distinct path is read
/// and compiled once; regions sharing a script share the compiled AST.
///
/// A missing or uncompilable script is a **hard spawn error** (fail fast,
/// before any tokens are spent): a hook that silently never ran would change
/// every inference with no signal. Runtime hook *eval* failures, by contrast,
/// warn and fall back per hook.
/// Compile every output validator the blueprint names, keyed by path.
///
/// A hard spawn error for the same reason a region script is: the moment to
/// discover an agent cannot check its own answer is not the end of a long run,
/// which is the only other time this script would ever be read.
///
/// Paths resolve against the blueprint directory, the same convention script
/// tools and region hooks use, so a validator travels with the agent that needs
/// it.
/// Resolve a blueprint-declared script path against the blueprint's directory,
/// refusing anything that lands outside it.
///
/// A script is code the blueprint ships, so it has no business living anywhere
/// but beside the blueprint. Without this check `base.join(declared)` happily
/// accepts `../../../../etc/shadow`: the file is read, handed to the Rhai
/// compiler, and whether it compiles becomes an oracle for what exists on the
/// host - from a package `lev add` installed, which `SECURITY.md` treats as a
/// real attacker. There is no `[read_paths]` fallback here on purpose: that
/// mechanism exists for an agent reading *data* the user pointed it at, and no
/// legitimate agent loads its own logic from outside its own directory.
pub(super) fn script_within_blueprint(
    base: &std::path::Path,
    declared: &str,
    what: &str,
) -> Result<std::path::PathBuf, String> {
    let full = base.join(declared);
    match leviath_core::resolves_within(&full, base) {
        true => Ok(full),
        false => Err(format!(
            "{what} '{declared}' resolves outside the blueprint's directory ({}); a script must \
             live beside the agent that declares it",
            base.display()
        )),
    }
}

pub(crate) fn resolve_output_validators(
    blueprint: &Blueprint,
    blueprint_path: &str,
) -> Result<HashMap<String, Arc<leviath_scripting::output_validator::OutputValidator>>, String> {
    let base = std::path::Path::new(blueprint_path)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let mut compiled = HashMap::new();

    let specs = blueprint
        .output
        .iter()
        .chain(blueprint.stages.iter().filter_map(|s| s.output.as_ref()));
    for spec in specs {
        let Some(script) = spec.validator.as_deref() else {
            continue;
        };
        if compiled.contains_key(script) {
            continue;
        }
        let path = script_within_blueprint(&base, script, "output validator")?;
        let source = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read output validator '{}': {e}", path.display()))?;
        let validator = leviath_scripting::output_validator::compile(script, &source)
            .map_err(|e| format!("output validator failed to compile: {e}"))?;
        compiled.insert(script.to_string(), Arc::new(validator));
    }
    Ok(compiled)
}

/// Compile every stage-hook script the blueprint declares, keyed by the path as
/// written.
///
/// Fail-fast at spawn, exactly as region scripts are: an unreadable file, one
/// that does not compile, or one the blueprint names for a hook it does not
/// define is a spawn error rather than a surprise partway through a run. One
/// file backing several hooks is read and compiled once.
///
/// Returns an empty map when no stage declares a hook, so the agent gets a
/// component that every lookup misses rather than a special case.
pub(crate) fn resolve_stage_hook_scripts(
    blueprint: &Blueprint,
    blueprint_path: &str,
) -> Result<HashMap<String, Arc<leviath_scripting::stage_hook::HookScript>>, String> {
    let base = std::path::Path::new(blueprint_path)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();

    // Gather what each file is wanted for before compiling, so a file backing
    // two hooks is checked for both in one pass.
    let mut wanted: HashMap<&str, Vec<&str>> = HashMap::new();
    for stage in &blueprint.stages {
        for (hook, path) in stage.hooks.declared() {
            wanted.entry(path).or_default().push(hook);
        }
    }

    let mut scripts = HashMap::new();
    for (path, hooks) in wanted {
        let full = script_within_blueprint(&base, path, "stage hook script")?;
        let source = std::fs::read_to_string(&full)
            .map_err(|e| format!("cannot read stage hook script '{}': {e}", full.display()))?;
        let compiled = leviath_scripting::stage_hook::compile(path, &source, &hooks)
            .map_err(|e| format!("stage hook script '{path}' failed to compile: {e}"))?;
        scripts.insert(path.to_string(), Arc::new(compiled));
    }
    Ok(scripts)
}

pub(crate) fn resolve_region_scripts(
    blueprint: &Blueprint,
    blueprint_path: &str,
) -> Result<HashMap<String, Arc<leviath_scripting::region_hook::RegionScript>>, String> {
    let base = std::path::Path::new(blueprint_path)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let mut scripts = HashMap::new();

    let layouts = std::iter::once(&blueprint.context_layout).chain(
        blueprint
            .stages
            .iter()
            .filter_map(|s| s.context_layout.as_ref()),
    );
    for layout in layouts {
        for region in &layout.regions {
            let leviath_core::RegionKind::Custom { script, .. } = &region.kind else {
                continue;
            };
            if scripts.contains_key(script) {
                continue;
            }
            let path = script_within_blueprint(&base, script, "custom region script")
                .map_err(|e| format!("region '{}': {e}", region.name))?;
            let source = std::fs::read_to_string(&path).map_err(|e| {
                format!(
                    "region '{}': cannot read custom region script '{}': {e}",
                    region.name,
                    path.display()
                )
            })?;
            let compiled =
                leviath_scripting::region_hook::compile(script, &source).map_err(|e| {
                    format!(
                        "region '{}': custom region script failed to compile: {e}",
                        region.name
                    )
                })?;
            scripts.insert(script.clone(), Arc::new(compiled));
        }
    }
    Ok(scripts)
}

/// Names already claimed by a built-in, sub-agent, or MCP tool - a discovered
/// script tool colliding with one of these is dropped (never shadows a core tool).
pub(super) fn reserved_tool_names(
    builtin_names: &HashSet<String>,
    mcp_tool_defs: &[Tool],
) -> HashSet<String> {
    let mut reserved: HashSet<String> = builtin_names.clone();
    reserved.extend(leviath_tools::BuiltinTools::subagent_tool_names());
    reserved.extend(mcp_tool_defs.iter().map(|t| t.name.clone()));
    reserved
}

/// Map a script's self-declared `@requires` capability name to the platform
/// [`ToolCapability`] it corresponds to. An unrecognized name returns `None`,
/// which the discovery pass treats as unsatisfiable (the tool is dropped) so a
/// typo can't silently slip a tool through the platform gate.
pub(super) fn script_cap(name: &str) -> Option<leviath_tools::ToolCapability> {
    match name {
        "network" | "net" | "http" => Some(leviath_tools::ToolCapability::Network),
        "shell" | "process" | "process_spawn" => Some(leviath_tools::ToolCapability::ProcessSpawn),
        "filesystem" | "file" | "fs" => Some(leviath_tools::ToolCapability::FileSystem),
        _ => None,
    }
}

/// Whether `platform` can satisfy every capability a script `@requires`. An
/// unknown capability name is never satisfiable.
pub(super) fn platform_satisfies_caps(
    platform: &leviath_tools::PlatformCapabilities,
    required_caps: &[String],
) -> bool {
    required_caps
        .iter()
        .all(|c| script_cap(c).is_some_and(|cap| platform.supports(cap)))
}

/// Whether the *current* platform can satisfy a script's `@requires` - the same
/// gate `discover_script_tools_in` applies at spawn. Exposed so the read-only CLI
/// surfaces (`lev tools`, `lev validate`, `lev mcp list`) report a tool's real
/// availability (and flag an unknown/typo'd capability) instead of listing a tool
/// the daemon would silently drop.
pub(crate) fn current_platform_satisfies(required_caps: &[String]) -> bool {
    platform_satisfies_caps(
        &leviath_tools::PlatformCapabilities::current(),
        required_caps,
    )
}

/// Discover and compile the script tools in `dirs`, returning the compiled set,
/// the routable names (collisions against `reserved` excluded), and the
/// advertised `Tool` defs.
///
/// A tool whose `@requires` capabilities the current platform can't satisfy is
/// dropped here (self-declared platform gating) - mirroring how
/// built-ins filter against [`PlatformCapabilities`].
pub(crate) fn discover_script_tools_in(
    dirs: &[std::path::PathBuf],
    reserved: &HashSet<String>,
) -> (leviath_scripting::ScriptToolSet, HashSet<String>, Vec<Tool>) {
    let (set, skipped) = leviath_scripting::ScriptToolSet::discover(dirs);
    for s in &skipped {
        // Pre-format the path to a plain string so the `tracing` field carries no
        // inline method call (an inline `%s.path.display()` leaves a macro
        // sub-region llvm-cov can't attribute even with the event enabled).
        let path = s.path.display().to_string();
        tracing::warn!(tool = %path, reason = %s.reason, "skipping invalid script tool");
    }
    let platform = leviath_tools::PlatformCapabilities::current();
    let mut names = HashSet::new();
    let mut defs = Vec::new();
    for meta in set.metas() {
        if reserved.contains(&meta.name) {
            tracing::warn!(tool = %meta.name, "script tool name collides with an existing tool - ignoring");
            continue;
        }
        if !platform_satisfies_caps(&platform, &meta.required_caps) {
            let caps = meta.required_caps.join(", ");
            tracing::warn!(tool = %meta.name, requires = %caps, "script tool requires a capability this platform lacks - ignoring");
            continue;
        }
        names.insert(meta.name.clone());
        defs.push(Tool {
            name: meta.name.clone(),
            description: meta.description.clone(),
            parameters: meta.parameters_schema(),
        });
    }
    (set, names, defs)
}

/// Discover the agent's Rhai script tools and build their `Tool`
/// defs (the spawn-time entry point). `extra_dir` adds the run workdir's `tools/`
/// for `dynamic_tools` agents.
pub(super) fn discover_script_tools(
    blueprint_path: &str,
    builtin_names: &HashSet<String>,
    mcp_tool_defs: &[Tool],
    extra_dir: Option<std::path::PathBuf>,
) -> (leviath_scripting::ScriptToolSet, HashSet<String>, Vec<Tool>) {
    let dirs = script_scan_dirs(blueprint_path, extra_dir);
    let reserved = reserved_tool_names(builtin_names, mcp_tool_defs);
    discover_script_tools_in(&dirs, &reserved)
}

/// The names a stage's `available_global_tools` grant expands to: every tool in
/// `set` whose source file lives under `tools_dir` (the global
/// `~/.leviath/tools/`) and that `surviving` still routes (not dropped for a
/// reserved-name collision or an unsatisfiable `@requires`).
///
/// The file location is the test, not the name. Scan order lets an agent's own
/// `tools/` or, for a `dynamic_tools` run, the workdir's `tools/` shadow a
/// global script under the same name, and that shadow is repository content.
/// Granting it because a trusted global tool happens to share the name would
/// let a checked-out repo put its own code behind a name the blueprint never
/// wrote, so a shadowed name is left out here and stays reachable only by an
/// explicit `available_tools` entry. No global directory (no home) means no
/// global grants. Sorted, so the model sees the same order every run.
pub(crate) fn global_tool_names(
    set: &leviath_scripting::ScriptToolSet,
    surviving: &HashSet<String>,
    tools_dir: Option<&std::path::Path>,
) -> Vec<String> {
    let Some(global) = tools_dir else {
        return Vec::new();
    };
    let mut names: Vec<String> = set
        .sources()
        .into_iter()
        .filter(|(meta, path)| path.starts_with(global) && surviving.contains(&meta.name))
        .map(|(meta, _)| meta.name)
        .collect();
    names.sort_unstable();
    names
}

/// A stage's Layer-1 grant list with its global grant applied: `available` as
/// written, followed by every name in `global_names` it did not already hold,
/// when `allow_global` is set; `available` unchanged otherwise.
///
/// Appending rather than merging keeps the blueprint's own order at the front,
/// and the de-duplication means a tool a stage names explicitly *and* has
/// installed globally is advertised once.
pub(crate) fn expand_global_grants(
    available: &[String],
    allow_global: bool,
    global_names: &[String],
) -> Vec<String> {
    let mut granted = available.to_vec();
    if allow_global {
        for name in global_names {
            if !granted.contains(name) {
                granted.push(name.clone());
            }
        }
    }
    granted
}
