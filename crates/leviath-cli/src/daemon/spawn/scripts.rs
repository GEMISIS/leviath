//! What scripts a blueprint brings, and whether they may run here.
//!
//! Rhai tools, region hooks, stage hooks and output validators are all code a
//! blueprint names, often as a file beside it, so every one of them is
//! something the blueprint author chose and this daemon is about to execute. Resolution and containment
//! therefore live together: `script_within_blueprint` is the fence, and nothing
//! below it loads a path that has not been through it.
//!
//! Platform capabilities are here for the same reason - a tool that declares a
//! capability the host does not provide is not advertised at all, which is a
//! decision about what runs rather than about how.

use super::*;
use leviath_runtime::spec::graph::{CodeRef, RegionKind, RunGraph};

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

/// Check every piece of code a graph names that `lev validate` promises to
/// find a fault in before a run starts: each custom region's script, each
/// output validator, and each stage hook.
///
/// Each is a hard spawn error, so the moment to find out one does not read or
/// compile is now, not partway through a run. A file is read from `base`, the
/// blueprint's directory, and never from outside it; one file named more than
/// once is read and compiled once.
pub(crate) fn check_graph_code(graph: &RunGraph, base: &std::path::Path) -> Result<(), String> {
    check_region_scripts(graph, base)?;
    check_output_validators(graph, base)?;
    check_stage_hooks(graph, base)
}

/// The label and source of some code a graph names: the path as written for
/// a file beside the blueprint, `inline` for code written in the graph. `what`
/// names the code in a message.
fn code_source(
    base: &std::path::Path,
    code: &CodeRef,
    what: &str,
) -> Result<(String, String), String> {
    match code {
        CodeRef::Inline(source) => Ok(("inline".to_string(), source.clone())),
        CodeRef::File(declared) => {
            let path = script_within_blueprint(base, declared, what)?;
            let source = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {what} '{}': {e}", path.display()))?;
            Ok((declared.clone(), source))
        }
    }
}

/// Every custom region's script, in the graph's layout and each stage's own.
fn check_region_scripts(graph: &RunGraph, base: &std::path::Path) -> Result<(), String> {
    let layouts =
        std::iter::once(&graph.layout).chain(graph.stages.iter().filter_map(|s| s.layout.as_ref()));
    let mut seen = HashSet::new();
    for region in layouts.flat_map(|l| l.regions.iter()) {
        let RegionKind::Custom { code, .. } = &region.kind else {
            continue;
        };
        if !seen.insert(code) {
            continue;
        }
        let (label, source) = code_source(base, code, "custom region script")
            .map_err(|e| format!("region '{}': {e}", region.name))?;
        leviath_scripting::region_hook::compile(&label, &source).map_err(|e| {
            format!(
                "region '{}': custom region script failed to compile: {e}",
                region.name
            )
        })?;
    }
    Ok(())
}

/// Every output validator: the graph's and each stage's.
fn check_output_validators(graph: &RunGraph, base: &std::path::Path) -> Result<(), String> {
    let validators = graph
        .output
        .iter()
        .chain(graph.stages.iter().filter_map(|s| s.output.as_ref()))
        .filter_map(|o| o.validator.as_ref());
    let mut seen = HashSet::new();
    for code in validators {
        if !seen.insert(code) {
            continue;
        }
        let (label, source) = code_source(base, code, "output validator")?;
        leviath_scripting::output_validator::compile(&label, &source)
            .map_err(|e| format!("output validator failed to compile: {e}"))?;
    }
    Ok(())
}

/// Every stage hook. The hooks one piece of code is named for are gathered
/// first, so it is compiled once and checked for every one of them: a file
/// named for a hook it does not define is refused, since that hook would
/// never run.
fn check_stage_hooks(graph: &RunGraph, base: &std::path::Path) -> Result<(), String> {
    let mut wanted: Vec<(&CodeRef, Vec<&str>)> = Vec::new();
    for (hook, code) in graph.stages.iter().flat_map(|s| s.hooks.iter()) {
        match wanted.iter_mut().find(|(c, _)| *c == code) {
            Some((_, hooks)) => hooks.push(hook),
            None => wanted.push((code, vec![hook])),
        }
    }
    for (code, hooks) in wanted {
        let (label, source) = code_source(base, code, "stage hook script")?;
        leviath_scripting::stage_hook::compile(&label, &source, &hooks)
            .map_err(|e| format!("stage hook script '{label}' failed to compile: {e}"))?;
    }
    Ok(())
}

/// Names already claimed by a built-in, sub-agent, or MCP tool - a discovered
/// script tool colliding with one of these is dropped (never shadows a core tool).
pub(crate) fn reserved_tool_names(
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
