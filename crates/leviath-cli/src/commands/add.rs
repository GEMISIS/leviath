//! `lev add` - Install an agent package

use clap::Args;
use std::path::Path;

use leviath_blueprint::FILE_NAME;
use leviath_core::policy::ToolPolicy;
use leviath_core::sandbox::SandboxKind;
use leviath_runtime::spec::graph::{RunGraph, ScriptPermission, Seed};

/// Arguments for `lev add`.
#[derive(Args)]
pub struct AddArgs {
    /// Path to an agent directory or a .leviath-bundle file
    #[arg(value_name = "PACKAGE")]
    pub package: String,
}

fn agents_dir_or_error(dir: Option<std::path::PathBuf>) -> anyhow::Result<std::path::PathBuf> {
    dir.ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))
}

/// Run `lev add`: install an agent from a directory or a bundle file.
pub(crate) async fn execute(args: AddArgs) -> anyhow::Result<()> {
    let installer = leviath_package::AgentInstaller::new();
    let agents_dir = resolve_agents_dir()?;
    // Best-effort, unlike `lev list`: a config that will not parse is a reason
    // to say less about the package being installed, never a reason to refuse
    // to install it.
    let config = crate::config::Config::load().ok();
    execute_with(&args, &installer, &agents_dir, config.as_ref()).await
}

/// Resolve `~/.leviath/agents`, the install root for `lev add`.
///
/// A thin wrapper over [`agents_dir_or_error`] supplying the real resolved
/// directory. The `#[cfg(test)]` guard below only lets tests force the
/// "no home directory" error arm of `execute()` deterministically - the real
/// the shared resolver can't be made to return `None` in any environment a
/// test may safely create (on macOS `dirs::home_dir()` falls back to a
/// passwd-database lookup independent of `$HOME`). It does NOT hide the real
/// body from coverage: with the toggle off, `agents_dir_or_error(
/// leviath_core::paths::agents_dir())` runs (and is measured) in every ordinary test. This
/// only computes a `PathBuf`; the `None` arm of `agents_dir_or_error` is
/// covered directly by `agents_dir_or_error_none_returns_error`.
fn resolve_agents_dir() -> anyhow::Result<std::path::PathBuf> {
    #[cfg(test)]
    if FORCE_AGENTS_DIR_ERROR.with(|f| f.get()) {
        anyhow::bail!("Could not determine home directory");
    }
    agents_dir_or_error(leviath_core::paths::agents_dir())
}

#[cfg(test)]
thread_local! {
    /// Test-only toggle letting `execute_returns_err_when_agents_dir_unresolvable`
    /// force `resolve_agents_dir`'s `Err` arm deterministically.
    static FORCE_AGENTS_DIR_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Core `lev add` logic, parameterized by installer + agents base directory
/// so it can be tested against tempdirs instead of the real
/// `~/.leviath/agents`.
async fn execute_with(
    args: &AddArgs,
    installer: &leviath_package::AgentInstaller,
    agents_dir: &Path,
    config: Option<&crate::config::Config>,
) -> anyhow::Result<()> {
    tracing::info!("Installing agent package");

    let package_path = Path::new(&args.package);
    if let Some(message) =
        crate::commands::run::locate::old_format(package_path, &args.package, Some(agents_dir))
    {
        anyhow::bail!(message);
    }

    if package_path.is_dir() {
        // Directory install: copy directory into <agents_dir>/<name>/
        install_from_dir(package_path, agents_dir, config)?;
    } else if package_path.exists() || args.package.ends_with(".leviath-bundle") {
        // Bundle file installation
        if !package_path.exists() {
            anyhow::bail!("Package file not found: {}", args.package);
        }
        println!("Installing from bundle: {}", args.package);
        let installed = installer.install(package_path)?;
        println!(
            "Installed agent '{}' v{} to {}",
            installed.name,
            installed.version,
            installed.path.display()
        );
        print_capabilities(&installed.name, &installed.path, config);
    } else {
        // Only local installs exist: agent directories and .leviath-bundle
        // files. Fail with a clear message rather than guessing at intent.
        anyhow::bail!(
            "'{}' is not a local agent directory or a .leviath-bundle file - \
             pass a path to one of those instead.",
            args.package
        );
    }

    Ok(())
}

/// The security-relevant things an agent package carries, as human-readable
/// lines.
///
/// A bare "Installed agent 'x' to …" would never tell the user that the
/// package ships executable `.rhai` tool scripts, pre-approves its own `shell`,
/// turns the sandbox off, or runs a command at spawn before any prompt. Every
/// one of those is a decision the user is making by installing, so `lev add`
/// must surface them.
///
/// Empty means the package declares nothing unusual - a plain prompt-and-stages
/// agent - in which case there is nothing to warn about and we stay quiet.
///
/// Pure over `(graph, script_tools, read_paths)` so the whole table is
/// testable without a filesystem or an installed agent. `graph` is `None` for
/// a package whose `agent.toml` does not read, which the installer reports on
/// its own and which has nothing to inventory. `read_paths` is the grant
/// report for this package under the active config, when one could be built;
/// without it the read-paths line falls back to stating the rule.
/// `may_loosen` is the operator's `[security] allow_blueprint_permissions`:
/// without it, a tool the blueprint pre-approves past its default is listed
/// as asked for rather than granted, since the runtime clamps it back.
pub(crate) fn describe_capabilities(
    graph: Option<&RunGraph>,
    script_tools: &[String],
    read_paths: Option<&crate::read_path_report::GrantReport>,
    may_loosen: bool,
) -> Vec<String> {
    let mut findings = Vec::new();
    let Some(graph) = graph else {
        return findings;
    };

    if !script_tools.is_empty() {
        findings.push(format!(
            "ships {} executable script tool(s): {}",
            script_tools.len(),
            script_tools.join(", ")
        ));
    }

    // Tool permissions the package grants itself, graph-wide or per stage.
    let mut granted: Vec<String> = std::iter::once(&graph.tool_permissions)
        .chain(graph.stages.iter().map(|s| &s.tool_permissions))
        .flatten()
        .filter(|(_, policy)| **policy == ToolPolicy::Allow)
        .map(|(tool, _)| tool.to_string())
        .collect();
    granted.sort();
    granted.dedup();
    // A blueprint may not loosen most tools past their default on its own:
    // the runtime clamps such a line back, so the tool still asks.
    let (clamped, effective): (Vec<String>, Vec<String>) = granted
        .into_iter()
        .partition(|tool| !may_loosen && clamped_by_default(tool));
    if !effective.is_empty() {
        findings.push(format!(
            "pre-approves these tools (no prompt at run time): {}",
            effective.join(", ")
        ));
    }
    if !clamped.is_empty() {
        findings.push(format!(
            "asks to pre-approve {}, which a blueprint cannot grant itself, so they still \
             ask at run time unless you allow them in config.toml or set [security] \
             allow_blueprint_permissions = true",
            clamped.join(", ")
        ));
    }

    // Script host functions it grants itself.
    let host = &graph.script_permissions;
    let allowed: Vec<&str> = [
        ("env_var", host.env_var),
        ("http_get", host.http_get),
        ("http_post", host.http_post),
        ("read_file", host.read_file),
        ("shell", host.shell),
        ("write_file", host.write_file),
    ]
    .into_iter()
    .filter(|(_, permission)| *permission == Some(ScriptPermission::Allow))
    .map(|(name, _)| name)
    .collect();
    if !allowed.is_empty() {
        findings.push(format!(
            "requests script host access: {}",
            allowed.join(", ")
        ));
    }

    // A sandbox opt-out, graph-wide or in any stage.
    let opts_out = std::iter::once(&graph.sandbox)
        .chain(graph.stages.iter().map(|s| &s.sandbox))
        .flatten()
        .any(|sandbox| sandbox.kind == SandboxKind::None);
    if opts_out {
        findings.push("asks to run tools directly on the host (sandbox = none)".to_string());
    }

    // Read paths beyond the workdir. Declaring is not granting - the entries
    // are inert until the user's config grants them - but the ask itself is
    // exactly what this inventory exists to surface.
    if !graph.read_paths.is_empty() {
        // With the active config in hand, say which of them are actually live
        // rather than repeating the rule and leaving the user to work it out.
        let status = match read_paths {
            Some(report) if report.has_ungranted() => format!(
                "; {} - grant the rest with [agent_read_paths.{}] in your config",
                report.summary(),
                report.agent
            ),
            Some(report) => format!("; {}, all granted by your config", report.summary()),
            None => "; inert unless you grant it via [security] read_paths / \
                     allow_blueprint_read_paths or [agent_read_paths.<name>] in your config"
                .to_string(),
        };
        findings.push(format!(
            "asks to read outside its workdir (read-only): {}{status}",
            graph.read_paths.join(", ")
        ));
    }

    // Command seeds run at spawn, before the first inference and therefore
    // before any approval prompt - the one place a blueprint executes
    // something without being asked.
    for command in seed_commands(graph) {
        findings.push(format!(
            "runs this command at startup, before any prompt: `{command}`"
        ));
    }

    findings
}

/// Whether the runtime clamps a blueprint's `allow` for `tool` back to the
/// tool's stricter default, the way [`crate::tools::resolve_policy`] does
/// when the operator has said nothing. The stage-control tools (`fan_out`,
/// `submit_output`) are carried out by the run itself and never ask, so they
/// are never clamped.
fn clamped_by_default(tool: &str) -> bool {
    use crate::tools::{blueprint_loosenable, default_tool_policy, restrictiveness};
    let canonical = leviath_tools::canonical_tool_name(tool);
    if leviath_tools::STAGE_CONTROL_TOOLS.contains(&canonical) {
        return false;
    }
    let default = default_tool_policy(tool, leviath_tools::is_builtin_tool(canonical));
    restrictiveness(default) > restrictiveness(ToolPolicy::Allow) && !blueprint_loosenable(tool)
}

/// Every `seed = { command = "..." }` in a graph, from the graph's layout and
/// every stage's own layout alike.
fn seed_commands(graph: &RunGraph) -> Vec<&str> {
    std::iter::once(&graph.layout)
        .chain(graph.stages.iter().filter_map(|s| s.layout.as_ref()))
        .flat_map(|layout| &layout.regions)
        .filter_map(|region| match &region.seed {
            Some(Seed::Command(command)) => Some(command.as_str()),
            _ => None,
        })
        .collect()
}

/// Print the capability inventory for a freshly installed agent, if it has one.
fn print_capabilities(name: &str, install_dir: &Path, config: Option<&crate::config::Config>) {
    let blueprint = crate::commands::run::locate::loaded_at(install_dir);
    let graph = blueprint.as_ref().map(|b| &b.graph);
    let scripts = script_tool_names(install_dir);
    let report = graph.and_then(|g| read_path_report(g, name, config));
    let may_loosen = config.is_some_and(|c| c.security.allow_blueprint_permissions);
    let findings = describe_capabilities(graph, &scripts, report.as_ref(), may_loosen);
    if findings.is_empty() {
        return;
    }
    println!("\n  '{name}' asks for the following. Review before running it:");
    for finding in &findings {
        println!("    - {finding}");
    }
    println!("  Inspect it with:  lev validate {name}");
}

/// The read-paths grant report for a just-installed graph, when there is a
/// config to judge it against.
///
/// The workdir a relative entry resolves against is the directory a `lev run`
/// would default to, which at install time is the one `lev add` was run from.
/// A broken grant list yields no report: the inventory falls back to stating
/// the rule, and `lev validate` says what is wrong with the config.
fn read_path_report(
    graph: &RunGraph,
    name: &str,
    config: Option<&crate::config::Config>,
) -> Option<crate::read_path_report::GrantReport> {
    let config = config?;
    let workdir = crate::commands::resolve_cwd().unwrap_or_default();
    crate::read_path_report::build(graph, name, config, &workdir)?.ok()
}

/// Names of the `.rhai` tool scripts an installed agent ships.
fn script_tool_names(install_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(install_dir.join("tools"))
        .into_iter()
        .flatten()
        .flatten()
        // `DirEntry::file_name` rather than `path().file_name()`: the latter
        // returns an `Option` that a directory entry can never actually be
        // missing, leaving an arm no test can reach.
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with(".rhai").then_some(name)
        })
        .collect();
    names.sort();
    names
}

/// Copy a plain agent directory into `<agents_dir>/<name>/`.
///
/// The agent name is the `[blueprint] name` of the directory's `agent.toml`
/// (falling back to the directory's own name).
fn install_from_dir(
    src: &Path,
    agents_dir: &Path,
    config: Option<&crate::config::Config>,
) -> anyhow::Result<()> {
    let manifest_path = src.join(FILE_NAME);
    if !manifest_path.exists() {
        anyhow::bail!(
            "No {FILE_NAME} found in '{}'. Is this an agent directory?",
            src.display()
        );
    }

    let content = std::fs::read_to_string(&manifest_path)?;
    let name = manifest_agent_name(&content)?.unwrap_or_else(|| {
        src.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    });
    // The name becomes a directory under the agents dir, so it has to be a
    // single path component. Joining it unchecked let `name = "../x"` remove
    // and overwrite whatever sat beside the agents directory.
    if !leviath_core::is_safe_path_component(&name) {
        anyhow::bail!(
            "invalid agent name '{name}': names may contain only letters, digits, \
             '.', '_' and '-'"
        );
    }

    let install_dir = agents_dir.join(&name);

    if install_dir.exists() {
        println!("Reinstalling agent '{}' (replacing existing)", name);
        std::fs::remove_dir_all(&install_dir)?;
    }

    copy_dir_recursive(src, &install_dir)?;
    println!("Installed agent '{}' to {}", name, install_dir.display());
    print_capabilities(&name, &install_dir, config);
    println!(
        "Run with:  {}",
        crate::commands::run::run_line(&name, "...")
    );
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Test-only toggle letting a test force the `Err` arm of a
    /// mid-iteration `ReadDir` entry deterministically (see
    /// [`unwrap_dir_entry`]) - the real failure mode (the directory handle
    /// becoming invalid mid-iteration: deleted out from under the process,
    /// an NFS ESTALE, or similar) is a genuine OS-level race that can't be
    /// reproduced deterministically across Linux/macOS/Windows CI.
    static FORCE_DIR_ENTRY_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Unwrap one `ReadDir` iteration result, with a test-only failure-injection
/// toggle (see [`FORCE_DIR_ENTRY_ERROR`]) so the `Err` arm - `ReadDir::next()`
/// failing after `read_dir` already succeeded in opening the directory --
/// can be exercised deterministically without needing to actually race the
/// filesystem.
fn unwrap_dir_entry(
    entry: std::io::Result<std::fs::DirEntry>,
) -> anyhow::Result<std::fs::DirEntry> {
    #[cfg(test)]
    if FORCE_DIR_ENTRY_ERROR.with(|f| f.get()) {
        anyhow::bail!("forced dir-entry error for testing");
    }
    Ok(entry?)
}

/// Recursively copy a directory tree, refusing links.
fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    copy_dir_recursive_with(src, dst, classify_entry)
}

/// What a directory entry is, judged by its own metadata rather than what it
/// might point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// A real directory: descend into it.
    Dir,
    /// A real file: copy it.
    File,
    /// A symlink, or something whose metadata could not be read: refuse it.
    Refused,
}

/// Classify an entry without following it. `symlink_metadata` is the point:
/// `is_dir()` follows a link, and copying through one would either pull in a
/// tree from outside the agent directory or copy a file the link resolves to.
fn classify_entry(path: &Path) -> Entry {
    match std::fs::symlink_metadata(path).map(|m| m.file_type()).ok() {
        Some(t) if t.is_dir() => Entry::Dir,
        Some(t) if !t.is_symlink() => Entry::File,
        _ => Entry::Refused,
    }
}

/// [`copy_dir_recursive`] with the entry classifier injected.
///
/// A `fn` pointer so there is one monomorphization. The seam exists because
/// the refusal has to be provable on every platform and a real symlink cannot
/// be created on the Windows CI runner; the installer in `leviath-package`
/// makes the same trade for the same reason.
fn copy_dir_recursive_with(
    src: &Path,
    dst: &Path,
    classify: fn(&Path) -> Entry,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = unwrap_dir_entry(entry)?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        match classify(&src_path) {
            Entry::Dir => copy_dir_recursive_with(&src_path, &dst_path, classify)?,
            Entry::File => {
                std::fs::copy(&src_path, &dst_path)?;
            }
            Entry::Refused => anyhow::bail!(
                "'{}' is a symlink or unreadable entry; an agent directory may not \
                 contain links",
                src_path.display()
            ),
        }
    }
    Ok(())
}

/// The `[blueprint] name` an `agent.toml` declares, if it declares a readable
/// one.
///
/// A file that is not TOML at all is refused rather than installed under a
/// guessed name. One whose `[blueprint]` table does not read (no name, no
/// version) gives no name, and the directory's own name stands in.
fn manifest_agent_name(content: &str) -> anyhow::Result<Option<String>> {
    toml::from_str::<toml::Table>(content)
        .map_err(|e| anyhow::anyhow!("{FILE_NAME} is not valid TOML: {e}"))?;
    Ok(leviath_blueprint::BlueprintMeta::read(content)
        .ok()
        .map(|meta| meta.name.to_string()))
}

#[cfg(test)]
mod capability_tests {
    use std::path::Path;

    use leviath_runtime::spec::graph::RunGraph;

    /// A stage and an empty layout: a graph that declares nothing unusual.
    const PLAIN: &str = "stages = [{ name = \"main\", system_prompt = \"p\" }]\n\
                         layout = { total_budget_tokens = 1000, regions = [] }\n";

    /// The `agent.toml` of a blueprint named `name` whose `[graph]` table is
    /// `graph`.
    fn blueprint(name: &str, graph: &str) -> String {
        format!("[blueprint]\nname = \"{name}\"\nversion = \"1.0.0\"\n\n[graph]\n{graph}")
    }

    /// The graph of [`blueprint`]`("x", graph)`, read.
    fn graph(graph: &str) -> RunGraph {
        leviath_blueprint::BlueprintFile::parse(&blueprint("x", graph))
            .expect("a test graph reads")
            .run_graph()
    }

    /// The inventory with no config to judge read paths against - the
    /// fallback wording. The grant-aware tests below pass a real report.
    fn describe_capabilities(graph_toml: &str, script_tools: &[String]) -> Vec<String> {
        super::describe_capabilities(Some(&graph(graph_toml)), script_tools, None, false)
    }

    /// A plain agent declares nothing unusual, so the inventory stays quiet -
    /// a warning that fires on everything teaches people to skip it.
    #[test]
    fn an_ordinary_agent_has_nothing_to_report() {
        assert!(describe_capabilities(PLAIN, &[]).is_empty());
    }

    #[test]
    fn script_tools_are_listed_by_name() {
        let findings = describe_capabilities(
            PLAIN,
            &["web_fetch.rhai".to_string(), "post.rhai".to_string()],
        );
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("2 executable script tool"));
        assert!(findings[0].contains("web_fetch.rhai"));
    }

    /// The case that matters most: a package that pre-approves its own shell.
    /// The runtime clamps that back unless the operator lets blueprints
    /// loosen, so it is reported as asked for, not granted; a tool a
    /// blueprint may pre-approve (or one already allowed) is a real grant.
    #[test]
    fn self_granted_tool_permissions_are_reported() {
        let toml = format!(
            "{PLAIN}tool_permissions = {{ shell = \"allow\", web_fetch = \"allow\", \
             fan_out = \"allow\", read_file = \"ask\" }}\n"
        );
        let findings = describe_capabilities(&toml, &[]);
        // The run carries out a fan-out itself; it never asks. `ask` is the
        // default posture, not a grant, so `read_file` is not listed.
        assert_eq!(
            findings,
            [
                "pre-approves these tools (no prompt at run time): fan_out, web_fetch",
                "asks to pre-approve shell, which a blueprint cannot grant itself, so they \
                 still ask at run time unless you allow them in config.toml or set \
                 [security] allow_blueprint_permissions = true",
            ]
        );

        // An operator who lets blueprints loosen makes it a real grant.
        let loosened = super::describe_capabilities(Some(&graph(&toml)), &[], None, true);
        assert_eq!(
            loosened,
            ["pre-approves these tools (no prompt at run time): fan_out, shell, web_fetch"]
        );
    }

    /// Script permissions that only *tighten* are not a grant, so they must
    /// not appear in the inventory - the same "quiet unless there is
    /// something to say" rule the ordinary-agent case establishes.
    #[test]
    fn script_permissions_that_grant_nothing_are_not_reported() {
        let graph = format!(
            "{PLAIN}script_permissions = {{ env_var = \"deny\", http_get = \"inherit\" }}\n"
        );
        assert!(
            describe_capabilities(&graph, &[]).is_empty(),
            "denying host access is not a capability to warn about"
        );
    }

    /// A grant in one stage, or the same tool granted twice, is listed once.
    #[test]
    fn stage_level_grants_are_reported_too() {
        let graph = "stages = [\n\
                     { name = \"build\", tool_permissions = { write_file = \"allow\" } },\n\
                     { name = \"check\", tool_permissions = { write_file = \"allow\" } },\n\
                     ]\n\
                     layout = { total_budget_tokens = 1000, regions = [] }\n";
        let findings = describe_capabilities(graph, &[]);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].matches("write_file").count(), 1, "{findings:?}");
    }

    #[test]
    fn script_host_grants_and_sandbox_opt_out_are_reported() {
        let graph = format!(
            "{PLAIN}script_permissions = {{ shell = \"allow\", http_post = \"allow\" }}\n\
             sandbox = {{ kind = \"none\" }}\n"
        );
        let findings = describe_capabilities(&graph, &[]);
        let joined = findings.join(" | ");
        assert!(
            joined.contains("script host access: http_post, shell"),
            "{joined}"
        );
        assert!(joined.contains("sandbox = none"), "{joined}");
    }

    /// A stage that opts out of the sandbox the graph asks for is an opt-out
    /// too.
    #[test]
    fn a_stage_sandbox_opt_out_is_reported() {
        let graph = "stages = [{ name = \"main\", sandbox = { kind = \"none\" } }]\n\
                     layout = { total_budget_tokens = 1000, regions = [] }\n\
                     sandbox = { kind = \"container\", image = \"ubuntu:24.04\" }\n";
        let findings = describe_capabilities(graph, &[]);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].contains("sandbox = none"), "{findings:?}");
    }

    /// A command seed runs at spawn - before the first inference and therefore
    /// before any approval prompt. It is the one thing a blueprint executes
    /// without being asked, so the exact command is shown.
    #[test]
    fn command_seeds_are_reported_verbatim() {
        let graph = "stages = [{ name = \"main\" }]\n\
                     layout = { total_budget_tokens = 1000, regions = [\n\
                     { name = \"repo\", kind = \"pinned\", budget = 500, seed = { command = \"git ls-files\" } },\n\
                     { name = \"notes\", kind = \"pinned\", budget = 500, seed = { literal = \"hi\" } },\n\
                     ] }\n";
        let findings = describe_capabilities(graph, &[]);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].contains("before any prompt"), "{findings:?}");
        assert!(findings[0].contains("git ls-files"), "{findings:?}");
    }

    #[test]
    fn stage_level_command_seeds_are_reported() {
        let graph = "stages = [{ name = \"discover\", layout = { total_budget_tokens = 500, regions = [\n\
                     { name = \"env\", kind = \"pinned\", budget = 500, seed = { command = \"curl https://evil\" } },\n\
                     ] } }]\n\
                     layout = { total_budget_tokens = 1000, regions = [] }\n";
        let findings = describe_capabilities(graph, &[]);
        assert!(findings[0].contains("curl https://evil"), "{findings:?}");
    }

    /// A sandbox the blueprint *opts into* is not a warning - only opting out
    /// is.
    #[test]
    fn opting_into_a_sandbox_is_not_reported() {
        let graph =
            format!("{PLAIN}sandbox = {{ kind = \"container\", image = \"ubuntu:24.04\" }}\n");
        assert!(describe_capabilities(&graph, &[]).is_empty());
    }

    /// `read_paths` is an ask to see beyond the workdir - listed verbatim,
    /// with the reminder that it stays inert until the user's config grants it.
    #[test]
    fn read_path_declarations_are_reported() {
        let graph =
            format!("{PLAIN}read_paths = [\"~/.leviath/runs\", \"glob:~/design-docs/**\"]\n");
        let findings = describe_capabilities(&graph, &[]);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].contains("read outside its workdir"),
            "{findings:?}"
        );
        assert!(findings[0].contains("~/.leviath/runs"), "{findings:?}");
        assert!(
            findings[0].contains("glob:~/design-docs/**"),
            "{findings:?}"
        );
        assert!(
            findings[0].contains("inert unless you grant it"),
            "{findings:?}"
        );
    }

    /// With a config to judge against, the inventory says which entries are
    /// live instead of restating the rule. This is what someone installing an
    /// agent on a fresh machine needs to know.
    #[test]
    fn read_path_declarations_carry_their_grant_status() {
        let graph = graph(&format!(
            "{PLAIN}read_paths = [\"/data/runs\", \"/data/docs\"]\n"
        ));

        let mut config = crate::config::Config::default();
        config.security.read_paths = vec!["/data/runs".to_string()];
        let partial = crate::read_path_report::build(&graph, "cto", &config, Path::new("/work"))
            .expect("declares read paths")
            .expect("grants compile");
        let findings = super::describe_capabilities(Some(&graph), &[], Some(&partial), false);
        assert!(
            findings[0].contains("2 declared, 1 granted"),
            "{findings:?}"
        );
        assert!(
            findings[0].contains("[agent_read_paths.cto]"),
            "{findings:?}"
        );

        config.security.read_paths.push("/data/docs".to_string());
        let full = crate::read_path_report::build(&graph, "cto", &config, Path::new("/work"))
            .expect("declares read paths")
            .expect("grants compile");
        let findings = super::describe_capabilities(Some(&graph), &[], Some(&full), false);
        assert!(findings[0].contains("all granted"), "{findings:?}");
    }

    /// An empty `read_paths` list asks for nothing - stay quiet.
    #[test]
    fn an_empty_read_paths_list_is_not_reported() {
        assert!(describe_capabilities(&format!("{PLAIN}read_paths = []\n"), &[]).is_empty());
    }

    /// Every way the grant report can be unavailable at install time: no
    /// config to judge against, nothing declared, and a config whose grants do
    /// not compile. Each falls back to stating the rule rather than guessing.
    #[test]
    fn no_grant_report_is_built_without_a_config_or_a_declaration() {
        let declaring = graph(&format!("{PLAIN}read_paths = [\"/data/runs\"]\n"));
        assert!(super::read_path_report(&declaring, "x", None).is_none());

        // A package that declares nothing has nothing to report either.
        let plain = graph(PLAIN);
        assert!(
            super::read_path_report(&plain, "x", Some(&crate::config::Config::default())).is_none()
        );

        // Nor does one whose grants cannot be compiled to judge it against.
        let mut broken = crate::config::Config::default();
        broken.security.read_paths = vec!["regex:relative/.*".to_string()];
        assert!(super::read_path_report(&declaring, "x", Some(&broken)).is_none());

        // And the ordinary case, so the fallbacks are not the only path tested.
        let report =
            super::read_path_report(&declaring, "x", Some(&crate::config::Config::default()))
                .expect("a declaration and a config give a report");
        assert_eq!(report.declared(), 1);
    }

    /// The `tools/` scan that feeds the inventory: only `.rhai` files count, and
    /// they come back sorted so the message is stable between runs.
    #[test]
    fn script_tool_names_lists_only_rhai_files_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        for name in ["zeta.rhai", "alpha.rhai", "README.md", "notes.txt"] {
            std::fs::write(tools.join(name), "x").unwrap();
        }
        assert_eq!(
            super::script_tool_names(dir.path()),
            vec!["alpha.rhai".to_string(), "zeta.rhai".to_string()]
        );
    }

    /// An agent with no `tools/` directory at all - the common case.
    #[test]
    fn script_tool_names_is_empty_without_a_tools_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(super::script_tool_names(dir.path()).is_empty());
    }

    /// The end-to-end printer, over a directory rather than a string: it must
    /// stay silent for an ordinary agent and speak for a demanding one, with a
    /// config to judge read paths against or without one.
    #[test]
    fn print_capabilities_reads_the_installed_directory() {
        crate::test_support::with_tracing(|| {
            let dir = tempfile::tempdir().unwrap();
            crate::test_support::write_test_agent(
                dir.path(),
                blueprint(
                    "q",
                    &format!(
                        "{PLAIN}tool_permissions = {{ shell = \"allow\" }}\n\
                         read_paths = [\"/data/runs\"]\n"
                    ),
                ),
            );
            let tools = dir.path().join("tools");
            std::fs::create_dir(&tools).unwrap();
            std::fs::write(tools.join("t.rhai"), "// @tool t\n").unwrap();
            super::print_capabilities("q", dir.path(), None);
            super::print_capabilities("q", dir.path(), Some(&crate::config::Config::default()));

            // And the quiet path: a plain agent prints nothing.
            let plain = tempfile::tempdir().unwrap();
            crate::test_support::write_test_agent(plain.path(), blueprint("p", PLAIN));
            super::print_capabilities("p", plain.path(), None);
        });
    }

    /// A package whose `agent.toml` does not read has nothing to inventory,
    /// even with script tools beside it: the installer reports the file.
    #[test]
    fn an_unreadable_blueprint_reports_nothing() {
        assert!(
            super::describe_capabilities(None, &["t.rhai".to_string()], None, false).is_empty()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{with_tracing, write_test_agent};

    /// The installers with no config to judge `[read_paths]` against, which is
    /// what every test here predating grant reporting assumed.
    async fn execute_with(
        args: &AddArgs,
        installer: &leviath_package::AgentInstaller,
        agents_dir: &Path,
    ) -> anyhow::Result<()> {
        super::execute_with(args, installer, agents_dir, None).await
    }

    fn install_from_dir(src: &Path, agents_dir: &Path) -> anyhow::Result<()> {
        super::install_from_dir(src, agents_dir, None)
    }

    // ─── agents_dir_or_error ─────────────────────────────────────────────

    #[test]
    fn agents_dir_or_error_some_returns_path() {
        let dir = std::path::PathBuf::from("/home/testuser/.leviath/agents");
        assert_eq!(agents_dir_or_error(Some(dir.clone())).unwrap(), dir);
    }

    #[test]
    fn agents_dir_or_error_none_returns_error() {
        let err = agents_dir_or_error(None).unwrap_err();
        assert!(
            err.to_string()
                .contains("Could not determine home directory")
        );
    }

    // ─── manifest_agent_name ──────────────────────────────────────────────

    #[test]
    fn manifest_agent_name_reads_the_blueprint_table() {
        let content = "[blueprint]\nname = \"my-agent\"\nversion = \"1.0\"\n";
        assert_eq!(
            manifest_agent_name(content).unwrap(),
            Some("my-agent".to_string())
        );
    }

    #[test]
    fn manifest_agent_name_is_none_when_absent_or_empty() {
        assert_eq!(manifest_agent_name("version = \"1.0\"\n").unwrap(), None);
        assert_eq!(
            manifest_agent_name("[blueprint]\nname = \"\"\nversion = \"1\"\n").unwrap(),
            None
        );
        // A `name` under another table is not the agent's name.
        assert_eq!(
            manifest_agent_name("[graph]\nname = \"x\"\n").unwrap(),
            None
        );
    }

    #[test]
    fn manifest_agent_name_rejects_bad_toml() {
        let err = manifest_agent_name("[blueprint\nname = 1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid TOML"), "{err}");
    }

    // ─── copy_dir_recursive ────────────────────────────────────────────────

    #[test]
    fn copy_dir_recursive_copies_files() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");

        std::fs::write(src_dir.path().join("file1.txt"), "hello").unwrap();
        std::fs::create_dir_all(src_dir.path().join("sub")).unwrap();
        std::fs::write(src_dir.path().join("sub/file2.txt"), "world").unwrap();

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();

        assert!(dst_path.join("file1.txt").exists());
        assert!(dst_path.join("sub/file2.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dst_path.join("file1.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst_path.join("sub/file2.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn copy_dir_recursive_empty_dir() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("empty-copy");

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();
        assert!(dst_path.exists());
        assert!(dst_path.is_dir());
    }

    #[test]
    fn copy_dir_recursive_nonexistent_src_errors() {
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("dst");
        let missing_src = dst_dir.path().join("does-not-exist");

        let result = copy_dir_recursive(&missing_src, &dst_path);
        assert!(result.is_err());
    }

    #[test]
    fn copy_dir_recursive_dst_parent_is_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("not-a-dir");
        std::fs::write(&file_path, "x").unwrap();
        let src = tempfile::tempdir().unwrap();
        let dst = file_path.join("child");

        let result = copy_dir_recursive(src.path(), &dst);
        assert!(result.is_err());
    }

    #[test]
    fn copy_dir_recursive_file_over_existing_dir_errors() {
        // Copying a file onto a destination path that already exists as a
        // directory fails on every platform (EISDIR / ERROR_ACCESS_DENIED),
        // exercising the `std::fs::copy(...)?` error arm.
        let src_dir = tempfile::tempdir().unwrap();
        std::fs::write(src_dir.path().join("clash"), "top secret").unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");
        // Pre-create dst/clash as a directory so the file copy collides.
        std::fs::create_dir_all(dst_path.join("clash")).unwrap();

        let result = copy_dir_recursive(src_dir.path(), &dst_path);
        assert!(result.is_err());
    }

    #[test]
    fn copy_dir_recursive_recursion_error_propagates() {
        // Exercises the recursive-call error-propagation branch OS-agnostically:
        // the destination already has a *file* where the recursion needs to
        // create a subdirectory, so the nested `create_dir_all` fails on every
        // platform and that `Err` bubbles up through the parent's
        // `copy_dir_recursive(...)?`.
        let src_dir = tempfile::tempdir().unwrap();
        let sub = src_dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("file.txt"), "data").unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");
        std::fs::create_dir_all(&dst_path).unwrap();
        // Block the recursion's create_dir_all(dst/sub) with a file at that path.
        std::fs::write(dst_path.join("sub"), "i am a file").unwrap();

        let result = copy_dir_recursive(src_dir.path(), &dst_path);
        assert!(result.is_err());
    }

    #[test]
    fn copy_dir_recursive_forced_mid_iteration_entry_error() {
        // Deterministically exercises `unwrap_dir_entry`'s `Err` arm (a real
        // `ReadDir::next()` failure mid-iteration) without racing the
        // filesystem, via the FORCE_DIR_ENTRY_ERROR test toggle.
        let src_dir = tempfile::tempdir().unwrap();
        std::fs::write(src_dir.path().join("file.txt"), "data").unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");

        FORCE_DIR_ENTRY_ERROR.with(|f| f.set(true));
        let result = copy_dir_recursive(src_dir.path(), &dst_path);
        FORCE_DIR_ENTRY_ERROR.with(|f| f.set(false));

        assert!(result.is_err());
    }

    #[test]
    fn unwrap_dir_entry_propagates_a_real_err_argument() {
        // `unwrap_dir_entry`'s own `Ok(entry?)` `?` still has a real error
        // arm distinct from the `FORCE_DIR_ENTRY_ERROR`-triggered early
        // `bail!` above it (that toggle short-circuits *before* this line
        // is ever reached) - `DirEntry` isn't constructible directly, but
        // its `Result` wrapper doesn't need a real one to test the `Err`
        // case: pass a synthetic `io::Error` straight in.
        let result = unwrap_dir_entry(Err(std::io::Error::other("synthetic entry error")));
        assert!(result.is_err());
    }

    // ─── install_from_dir ──────────────────────────────────────────────────

    /// The classifier judges an entry by its own metadata: a directory, a
    /// plain file, and a path that has no metadata at all.
    #[test]
    fn classify_entry_tells_dirs_files_and_missing_apart() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("tools");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("web_fetch.rhai"), "fn main() {}").unwrap();
        assert_eq!(classify_entry(&nested), Entry::Dir);
        assert_eq!(classify_entry(&nested.join("web_fetch.rhai")), Entry::File);
        assert_eq!(
            classify_entry(&dir.path().join("no-such-entry")),
            Entry::Refused
        );
    }

    /// A real symlink, where the platform can make one: judged by the link's
    /// own metadata, so even a link to an ordinary file inside the directory
    /// is refused rather than copied through.
    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_refuses_a_real_symlink() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("real.txt"), "fine").unwrap();
        std::os::unix::fs::symlink(src.path().join("real.txt"), src.path().join("link.txt"))
            .unwrap();
        assert_eq!(classify_entry(&src.path().join("link.txt")), Entry::Refused);

        let dst = tempfile::tempdir().unwrap();
        let err = copy_dir_recursive(src.path(), &dst.path().join("copy"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("link.txt"), "{err}");
    }

    /// An agent directory may not contain a link: the classifier is injected
    /// because a real symlink cannot be created on every CI platform, and the
    /// refusal has to be provable everywhere.
    #[test]
    fn copy_dir_recursive_refuses_a_link_or_unreadable_entry() {
        fn refuse_links(path: &Path) -> Entry {
            if path.file_name().is_some_and(|n| n == "link.txt") {
                Entry::Refused
            } else {
                classify_entry(path)
            }
        }
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("ok.txt"), "fine").unwrap();
        std::fs::write(src.path().join("link.txt"), "pretend I am a symlink").unwrap();
        // The injected classifier defers to the real one for everything else;
        // asked directly, because `read_dir` order decides whether the copy
        // reaches `ok.txt` before it bails on the link.
        assert_eq!(refuse_links(&src.path().join("ok.txt")), Entry::File);
        let dst = tempfile::tempdir().unwrap();

        let err = copy_dir_recursive_with(src.path(), &dst.path().join("copy"), refuse_links)
            .unwrap_err()
            .to_string();
        assert!(err.contains("link.txt"), "{err}");
        assert!(err.contains("symlink"), "{err}");
    }

    /// A manifest whose name walks out of the agents directory must not be
    /// installed, and must not touch what it points at: joining the name onto
    /// the agents directory and clearing the target before copying would
    /// delete whatever the walk landed on.
    #[test]
    fn install_from_dir_refuses_a_name_that_escapes_the_agents_dir() {
        let root = tempfile::tempdir().unwrap();
        let agents_dir = root.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let victim = root.path().join("escaped");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keepme.txt"), "precious").unwrap();

        let src = root.path().join("evil");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("agent.toml"),
            "[blueprint]\nname = \"../escaped\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();

        let err = install_from_dir(&src, &agents_dir).unwrap_err().to_string();
        assert!(err.contains("invalid agent name"), "{err}");
        assert_eq!(
            std::fs::read_to_string(victim.join("keepme.txt")).unwrap(),
            "precious",
            "the escape target must be left alone"
        );
        assert!(!agents_dir.join("agent.toml").exists());
    }

    /// The name comes from the `[blueprint]` table, not from the first line
    /// that happens to start with `name`: a `name` key elsewhere in the file
    /// is not the agent's name.
    #[test]
    fn install_from_dir_reads_the_blueprint_table_not_the_first_name_line() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("tabled");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("agent.toml"),
            "[blueprint]\nversion = \"1.0\"\n\n[graph]\nstages = [{ name = \"x\" }]\n",
        )
        .unwrap();
        let agents_dir = tempfile::tempdir().unwrap();

        install_from_dir(&src, agents_dir.path()).unwrap();

        let installed: Vec<String> = std::fs::read_dir(agents_dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(installed, vec!["tabled".to_string()], "{installed:?}");
    }

    /// A manifest that is not TOML is refused rather than installed under a
    /// guessed name.
    #[test]
    fn install_from_dir_refuses_a_manifest_that_is_not_toml() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("broken");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("agent.toml"), "[blueprint\nname = \"x\"\n").unwrap();
        let agents_dir = tempfile::tempdir().unwrap();

        let err = install_from_dir(&src, agents_dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid TOML"), "{err}");
        assert!(
            std::fs::read_dir(agents_dir.path())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn install_from_dir_no_manifest_errors() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = tempfile::tempdir().unwrap();
        let result = install_from_dir(dir.path(), agents_dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("agent.toml"));
    }

    #[test]
    fn install_from_dir_copies_and_names_from_manifest() {
        let src = tempfile::tempdir().unwrap();
        let agents_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("agent.toml"),
            "[blueprint]\nname = \"my-agent\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        std::fs::write(src.path().join("extra.txt"), "data").unwrap();

        install_from_dir(src.path(), agents_dir.path()).unwrap();

        let installed_dir = agents_dir.path().join("my-agent");
        assert!(installed_dir.join("agent.toml").exists());
        assert!(installed_dir.join("extra.txt").exists());
    }

    #[test]
    fn install_from_dir_falls_back_to_dirname_when_name_missing() {
        let src = tempfile::tempdir().unwrap();
        let agent_dir = src.path().join("my-dir-name");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.toml"), "version = \"1.0\"\n").unwrap();
        let agents_dir = tempfile::tempdir().unwrap();

        install_from_dir(&agent_dir, agents_dir.path()).unwrap();

        assert!(agents_dir.path().join("my-dir-name").exists());
    }

    #[test]
    fn install_from_dir_reinstalls_existing() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("agent.toml"),
            "[blueprint]\nname = \"dup-agent\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        let agents_dir = tempfile::tempdir().unwrap();

        // Pre-create an existing install with a stale file that should be wiped.
        let existing = agents_dir.path().join("dup-agent");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("stale.txt"), "old").unwrap();

        install_from_dir(src.path(), agents_dir.path()).unwrap();

        assert!(!existing.join("stale.txt").exists());
        assert!(existing.join("agent.toml").exists());
    }

    #[test]
    fn install_from_dir_invalid_utf8_manifest_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.toml"), [0xFF, 0xFE, 0xFA]).unwrap();
        let agents_dir = tempfile::tempdir().unwrap();

        let result = install_from_dir(dir.path(), agents_dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn install_from_dir_remove_dir_all_failure_errors() {
        // The existing install target is a *file*, so `exists()` passes the
        // reinstall guard but `remove_dir_all` (which requires a directory)
        // fails on every platform, exercising that `?` arm.
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("agent.toml"),
            "[blueprint]\nname = \"file-agent\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();

        let agents_dir = tempfile::tempdir().unwrap();
        std::fs::write(agents_dir.path().join("file-agent"), "not a dir").unwrap();

        let result = install_from_dir(src.path(), agents_dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn install_from_dir_copy_failure_propagates() {
        // `agents_dir` is itself a *file*, so `copy_dir_recursive`'s
        // `create_dir_all` for the install target (a child path of a file)
        // fails on every platform, and that `Err` propagates through
        // `install_from_dir`'s `copy_dir_recursive(...)?`.
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("agent.toml"),
            "[blueprint]\nname = \"broken-copy-agent\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        std::fs::write(src.path().join("extra.txt"), "data").unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let agents_file = tmp.path().join("agents-is-a-file");
        std::fs::write(&agents_file, "not a dir").unwrap();

        let result = install_from_dir(src.path(), &agents_file);
        assert!(result.is_err());
    }

    // ─── execute_with: directory + bundle-file paths ───────────────────────

    #[test]
    fn execute_with_directory_package_installs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let src = tempfile::tempdir().unwrap();
                std::fs::write(
                    src.path().join("agent.toml"),
                    crate::test_support::tiny_blueprint("dir-pkg"),
                )
                .unwrap();
                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: src.path().to_str().unwrap().to_string(),
                };

                execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap();

                assert!(agents_dir.path().join("dir-pkg").exists());
            })
        });
    }

    #[test]
    fn execute_with_directory_without_manifest_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let src = tempfile::tempdir().unwrap(); // no agent.toml inside
                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: src.path().to_str().unwrap().to_string(),
                };

                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("agent.toml"));
            })
        });
    }

    #[test]
    fn execute_with_missing_bundle_file_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: "nonexistent.leviath-bundle".to_string(),
                };

                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("Package file not found"));
            })
        });
    }

    #[test]
    fn execute_with_bundle_file_installs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let project_dir = tempfile::tempdir().unwrap();
                std::fs::write(
                    project_dir.path().join("agent.toml"),
                    crate::test_support::tiny_blueprint("bundled-pkg"),
                )
                .unwrap();
                let bundle_bytes = leviath_package::AgentBundler::new()
                    .bundle(project_dir.path())
                    .unwrap();
                let bundle_dir = tempfile::tempdir().unwrap();
                // Named the way `lev pack` names its output: the install is
                // still called what the manifest says, not what the file is.
                let bundle_path = bundle_dir.path().join("bundled-pkg-1.0.0.leviath-bundle");
                std::fs::write(&bundle_path, bundle_bytes).unwrap();

                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: bundle_path.to_str().unwrap().to_string(),
                };

                execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap();

                assert!(agents_dir.path().join("bundled-pkg").exists());
                assert!(!agents_dir.path().join("bundled-pkg-1.0.0").exists());
            })
        });
    }

    #[test]
    fn execute_with_corrupt_bundle_file_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let bundle_dir = tempfile::tempdir().unwrap();
                let bundle_path = bundle_dir.path().join("broken.leviath-bundle");
                std::fs::write(&bundle_path, b"not a valid gzip archive").unwrap();

                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: bundle_path.to_str().unwrap().to_string(),
                };

                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("Failed to extract package"));
            })
        });
    }

    /// An `agent.leviath` an earlier release wrote, by its directory or the
    /// file, says how to convert it, and nothing is installed.
    #[test]
    fn execute_with_an_old_manifest_says_how_to_convert_it() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let agents_dir = tempfile::tempdir().unwrap();
            let src = tempfile::tempdir().unwrap();
            let old = src.path().join("agent.leviath");
            std::fs::write(&old, "[agent]\nname = \"old\"\n").unwrap();
            let installer =
                leviath_package::AgentInstaller::with_install_dir(agents_dir.path().to_path_buf());
            for package in [src.path(), old.as_path()] {
                let args = AddArgs {
                    package: package.display().to_string(),
                };
                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("lev blueprint migrate"), "{err}");
            }
            assert_eq!(std::fs::read_dir(agents_dir.path()).unwrap().count(), 0);
        });
    }

    #[test]
    fn execute_with_unrecognized_package_reports_local_only() {
        // A package that is neither a local directory nor a .leviath-bundle
        // file must fail with a clear message, never a network attempt.
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: "some-registry-agent".to_string(),
                };
                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(
                    err.to_string()
                        .contains("not a local agent directory or a .leviath-bundle file"),
                    "expected the v1-cut message, got: {err}"
                );
            })
        });
    }

    // ─── path detection ────────────────────────────────────────────────────

    #[test]
    fn bundle_extension_detected() {
        let package = "my-agent-1.0.leviath-bundle";
        assert!(package.ends_with(".leviath-bundle"));
    }

    #[test]
    fn directory_path_detected() {
        let dir = tempfile::tempdir().unwrap();
        let package_path = Path::new(dir.path().to_str().unwrap());
        assert!(package_path.is_dir());
    }

    #[test]
    fn registry_name_not_dir_not_bundle() {
        let package = "my-cool-agent";
        let package_path = Path::new(package);
        assert!(!package_path.is_dir());
        assert!(!package.ends_with(".leviath-bundle"));
    }

    // ─── copy_dir_recursive with nested dirs ──────────────────────────────

    #[test]
    fn copy_dir_recursive_deeply_nested() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("deep-copy");

        std::fs::create_dir_all(src_dir.path().join("a/b/c")).unwrap();
        std::fs::write(src_dir.path().join("a/b/c/deep.txt"), "deep").unwrap();

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();

        assert!(dst_path.join("a/b/c/deep.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dst_path.join("a/b/c/deep.txt")).unwrap(),
            "deep"
        );
    }

    // ─── execute(): real entry point wrapper ───────────────────────────────

    #[test]
    fn execute_real_wrapper_fails_fast_without_touching_real_agents_dir() {
        // Drives the real `execute()` (dirs::home_dir() + AgentInstaller::new()
        // + delegation to execute_with) - safe because a nonexistent
        // ".leviath-bundle" path bails out in execute_with's "Package file
        // not found" check before any real file under ~/.leviath/agents is
        // ever touched.
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                // Isolated: `execute` reads the active config, to report
                // `[read_paths]` grant status on what it installs.
                crate::config::with_isolated_config_path_async("add-real-wrapper", |_fake| async {
                    let args = AddArgs {
                        package: "definitely-not-a-real-bundle-xyz.leviath-bundle".to_string(),
                    };
                    let err = execute(args).await.unwrap_err();
                    assert!(err.to_string().contains("Package file not found"));
                })
                .await;
            })
        });
    }

    #[test]
    fn execute_returns_err_when_agents_dir_unresolvable() {
        // Drives `execute`'s `resolve_agents_dir()?` error-propagation
        // branch for real via the test-only `FORCE_AGENTS_DIR_ERROR` toggle
        // on `resolve_agents_dir`'s twin (see its doc comment for why the
        // real implementation's failure can't be forced directly).
        let rt = tokio::runtime::Runtime::new().unwrap();
        FORCE_AGENTS_DIR_ERROR.with(|f| f.set(true));
        let result = rt.block_on(async {
            let args = AddArgs {
                package: "whatever.leviath-bundle".to_string(),
            };
            execute(args).await
        });
        FORCE_AGENTS_DIR_ERROR.with(|f| f.set(false));

        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("Could not determine home directory")
        );
    }

    // ─── install_from_dir with valid manifest ─────────────────────────────

    #[test]
    fn install_from_dir_with_manifest_runs() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
[blueprint]
name = "test-install-agent-xyz"
version = "0.1.0"
description = "test"
"#;
        write_test_agent(dir.path(), manifest);
        std::fs::write(dir.path().join("readme.txt"), "hello").unwrap();

        let agents_dir = tempfile::tempdir().unwrap();
        install_from_dir(dir.path(), agents_dir.path()).unwrap();

        let install_dir = agents_dir.path().join("test-install-agent-xyz");
        assert!(install_dir.join("agent.toml").exists());
        assert!(install_dir.join("readme.txt").exists());
    }
}
