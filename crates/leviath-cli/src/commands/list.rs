//! `lev list` - List available agents and blueprints

use clap::Args;
use std::fs;
use std::path::{Path, PathBuf};

use super::resolve_cwd;
use crate::config::Config;
use leviath_core::manifest::parse_manifest;

/// Which half of the catalog `lev list` reports.
///
/// A `ValueEnum` rather than a string: the flag accepted anything and read
/// nothing, so `--filter agants` was indistinguishable from `--filter agents`
/// and both printed everything. clap now rejects a spelling it does not know,
/// naming the ones it does.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum ListFilter {
    /// Everything: runnable agents, then the bundled catalog.
    All,
    /// Only agents you can run right now - installed, configured, or local.
    Agents,
    /// Only the blueprints bundled with this binary, which `lev setup` installs.
    Blueprints,
}

impl ListFilter {
    /// Whether runnable agents (installed, configured, local) are reported.
    fn shows_agents(self) -> bool {
        matches!(self, Self::All | Self::Agents)
    }

    /// Whether the bundled blueprint catalog is reported.
    fn shows_blueprints(self) -> bool {
        matches!(self, Self::All | Self::Blueprints)
    }
}

/// Arguments for `lev list`.
#[derive(Args)]
pub struct ListArgs {
    /// Filter by type
    #[arg(short, long, value_enum, default_value_t = ListFilter::All)]
    pub filter: ListFilter,

    /// Report the catalog as JSON instead of prose, with each agent's source
    /// named rather than implied by a heading.
    #[arg(long)]
    pub json: bool,
}

/// Info parsed from an agent manifest for display.
#[derive(serde::Serialize)]
pub(crate) struct AgentInfo {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) description: String,
    /// The agent's `[read_paths]` grant status under the active config, when it
    /// declares any. Shown because a declaration nothing grants is inert, and
    /// the listing is where someone looks before running an agent they just
    /// installed or copied over from another machine.
    read_paths: Option<String>,
}

/// One agent in `lev list --json`, with the source the prose report puts in a
/// heading and the path `lev run` would resolve.
#[derive(serde::Serialize)]
pub(crate) struct ListedAgent {
    #[serde(flatten)]
    pub(crate) info: AgentInfo,
    /// `installed`, `configured`, or `local`.
    pub(crate) source: &'static str,
    pub(crate) path: String,
}

/// What `lev list --json` prints.
#[derive(serde::Serialize)]
pub(crate) struct ListReport {
    /// Every agent that can be run by name or path right now.
    pub(crate) agents: Vec<ListedAgent>,
    /// The catalog embedded in this binary, which `lev setup` installs from.
    /// Not runnable until installed, which is why it is a separate key.
    pub(crate) bundled: Vec<BundledEntry>,
}

#[derive(serde::Serialize)]
pub(crate) struct BundledEntry {
    pub(crate) name: String,
    pub(crate) version: String,
}

fn read_agent_info(manifest_path: &Path, config: &Config, cwd: &Path) -> Option<AgentInfo> {
    let content = fs::read_to_string(manifest_path).ok()?;
    let blueprint = parse_manifest(&content).ok()?;
    let read_paths = read_path_summary(&blueprint, config, cwd);
    Some(AgentInfo {
        name: blueprint.name,
        version: blueprint.version,
        description: blueprint.description,
        read_paths,
    })
}

/// The one-line `[read_paths]` verdict for an agent, or `None` when it declares
/// none. A config whose own grant list is broken says so here rather than
/// staying silent; `lev validate` and the spawn error carry the detail.
fn read_path_summary(
    blueprint: &leviath_core::Blueprint,
    config: &Config,
    cwd: &Path,
) -> Option<String> {
    match crate::read_path_report::build(blueprint, config, cwd)? {
        Ok(report) if report.has_ungranted() => Some(format!(
            "read_paths: {} - `lev validate` shows which",
            report.summary()
        )),
        Ok(report) => Some(format!("read_paths: {}", report.summary())),
        Err(e) => Some(format!("read_paths: {e}")),
    }
}

fn scan_directory_for_agents(dir: &Path, config: &Config, cwd: &Path) -> Vec<(PathBuf, AgentInfo)> {
    let mut agents = Vec::new();
    if !dir.exists() {
        return agents;
    }

    // Check if this directory itself has an agent.leviath
    let direct_manifest = dir.join(leviath_core::files::MANIFEST_FILENAME);
    if direct_manifest.exists()
        && let Some(info) = read_agent_info(&direct_manifest, config, cwd)
    {
        agents.push((dir.to_path_buf(), info));
    }

    // Check subdirectories
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let manifest_path = path.join(leviath_core::files::MANIFEST_FILENAME);
                if manifest_path.exists()
                    && let Some(info) = read_agent_info(&manifest_path, config, cwd)
                {
                    agents.push((path, info));
                }
            }
        }
    }

    agents
}

/// One agent's listing: the name line every section shares, plus the
/// `[read_paths]` line when there is one.
fn print_agent(info: &AgentInfo) {
    let desc = if info.description.is_empty() {
        String::new()
    } else {
        format!(" - {}", info.description)
    };
    println!("  {} (v{}){}", info.name, info.version, desc);
    if let Some(read_paths) = &info.read_paths {
        println!("      {read_paths}");
    }
}

/// Run `lev list`: show the installed agents.
pub(crate) async fn execute(args: ListArgs) -> anyhow::Result<()> {
    // Propagate, don't default: a config that exists but doesn't parse would
    // silently list from the default `agent_paths`, hiding the user's own
    // agent directories with no hint why (a missing file loads as defaults).
    let config = Config::load()?;
    let agents_dir = get_agents_dir()?;
    let cwd = resolve_cwd().unwrap_or_default();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));

    match args.json {
        true => json_agent_listing(&agents_dir, &cwd, &config, args.filter),
        false => print_agent_listing(&agents_dir, &cwd, exe_dir.as_deref(), &config, args.filter),
    }
}

/// `lev list --json`: the same three runnable sources the prose report walks,
/// each agent tagged with where it came from and the path `lev run` resolves.
///
/// The on-disk `<exe_dir>/agents` scan the prose report folds into its bundled
/// line is left out: those entries are not installed, and merging them into a
/// name-and-version list loses which of the two a name came from.
fn json_agent_listing(
    agents_dir: &Path,
    cwd: &Path,
    config: &Config,
    filter: ListFilter,
) -> anyhow::Result<()> {
    let report = build_list_report(agents_dir, cwd, config, filter);
    // Owned strings with no map keys to reject, so this cannot fail.
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("an agent listing serializes")
    );
    Ok(())
}

/// The report [`json_agent_listing`] prints. Split out so its contents are
/// assertable without capturing stdout, and shared with the dashboard's
/// new-run picker so the two offer exactly the same agents.
pub(crate) fn build_list_report(
    agents_dir: &Path,
    cwd: &Path,
    config: &Config,
    filter: ListFilter,
) -> ListReport {
    // An excluded half is reported as an empty list rather than a missing key,
    // so a consumer indexes the same shape whatever the filter was.
    if !filter.shows_agents() {
        return ListReport {
            agents: Vec::new(),
            bundled: bundled_entries(),
        };
    }
    let installed = scan_directory_for_agents(agents_dir, config, cwd);
    let local = read_agent_info(
        &cwd.join(leviath_core::files::MANIFEST_FILENAME),
        config,
        cwd,
    );
    let configured: Vec<(PathBuf, AgentInfo)> = config
        .agent_paths
        .iter()
        .flat_map(|dir| scan_directory_for_agents(dir, config, cwd))
        .collect();

    let from = |entries: Vec<(PathBuf, AgentInfo)>, source| {
        entries.into_iter().map(move |(path, info)| ListedAgent {
            info,
            source,
            path: path.display().to_string(),
        })
    };
    let mut agents: Vec<ListedAgent> = from(installed, "installed")
        .chain(from(configured, "configured"))
        .collect();
    if let Some(info) = local {
        agents.push(ListedAgent {
            info,
            source: "local",
            path: cwd
                .join(leviath_core::files::MANIFEST_FILENAME)
                .display()
                .to_string(),
        });
    }

    ListReport {
        agents,
        bundled: match filter.shows_blueprints() {
            true => bundled_entries(),
            false => Vec::new(),
        },
    }
}

/// The embedded blueprint catalog, as report entries.
fn bundled_entries() -> Vec<BundledEntry> {
    crate::bundled::BUNDLED_AGENTS
        .iter()
        .map(|a| BundledEntry {
            name: a.name.to_string(),
            version: a.version.to_string(),
        })
        .collect()
}

/// Core `lev list` logic, parameterized by every real-environment source it
/// reads from so it can be tested against tempdirs instead of the real
/// home directory / CWD / executable location / config.
fn print_agent_listing(
    agents_dir: &Path,
    cwd: &Path,
    exe_dir: Option<&Path>,
    config: &Config,
    filter: ListFilter,
) -> anyhow::Result<()> {
    // Tracks whether the user has any agent they can actually *run*. The
    // bundled catalog deliberately does not count: it is always non-empty, and
    // treating it as "you have agents" would suppress the get-started guidance
    // for exactly the person who needs it - someone with a fresh install and
    // nothing installed yet.
    let mut found_runnable = false;

    // 1. Installed agents (~/.leviath/agents/)
    let installed = match filter.shows_agents() {
        true => scan_directory_for_agents(agents_dir, config, cwd),
        false => Vec::new(),
    };
    if !installed.is_empty() {
        found_runnable = true;
        println!("Installed agents (~/.leviath/agents/):");
        for (_path, info) in &installed {
            print_agent(info);
        }
        println!();
    }

    // 2. Local (current directory)
    let local_manifest = cwd.join(leviath_core::files::MANIFEST_FILENAME);
    if filter.shows_agents()
        && local_manifest.exists()
        && let Some(info) = read_agent_info(&local_manifest, config, cwd)
    {
        found_runnable = true;
        println!("Local (current directory):");
        print_agent(&info);
        println!();
    }

    // 3. Config's agent_paths directories
    let mut config_agents = Vec::new();
    if filter.shows_agents() {
        for agent_path in &config.agent_paths {
            let found = scan_directory_for_agents(agent_path, config, cwd);
            config_agents.extend(found);
        }
    }
    if !config_agents.is_empty() {
        found_runnable = true;
        println!("From configured paths:");
        for (_path, info) in &config_agents {
            print_agent(info);
        }
        println!();
    }

    // 4. Bundled agents - the blueprints embedded in this binary.
    //
    // Reports the embedded catalog, which is what `lev setup` installs from.
    // Scanning only `<exe_dir>/agents` would leave this section blank outside a
    // git checkout - a directory no real install has. The on-disk scan stays as
    // a second source so a checkout or a packaging layout that *does* ship an
    // `agents/` dir next to the binary still shows up.
    let mut builtin_names: Vec<String> = match filter.shows_blueprints() {
        true => crate::bundled::BUNDLED_AGENTS
            .iter()
            .map(|a| format!("{} (v{})", a.name, a.version))
            .collect(),
        false => Vec::new(),
    };
    if filter.shows_blueprints()
        && let Some(exe_dir) = exe_dir
    {
        for (_path, info) in scan_directory_for_agents(&exe_dir.join("agents"), config, cwd) {
            let entry = format!("{} (v{})", info.name, info.version);
            if !builtin_names.contains(&entry) {
                builtin_names.push(entry);
            }
        }
    }
    // The embedded catalog is always populated (a build that found no
    // blueprints fails `bundled`'s own invariant test), so this is keyed on the
    // filter rather than on emptiness - the latter would be a branch that can
    // never be false.
    if filter.shows_blueprints() {
        println!("Bundled agents (install with `lev setup`):");
        println!("  {}", builtin_names.join(", "));
        println!();
    }

    // Only when the listing was reporting runnable agents at all. Under
    // `--filter blueprints` there is nothing to conclude from their absence,
    // and telling someone to run `lev setup` because they asked to see the
    // catalog would be a non sequitur.
    if filter.shows_agents() && !found_runnable {
        println!("No agent blueprints installed yet.");
        println!();
        println!("To install the bundled agent blueprints:");
        println!("  lev setup");
        println!();
        println!("To create your own:");
        println!("  lev create my-agent");
    }

    Ok(())
}

/// Core `get_agents_dir` logic, parameterized by the home directory so the
/// "could not determine home directory" error path can be unit tested
/// without depending on the real environment.
fn get_agents_dir_or_error(dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    dir.ok_or(anyhow::anyhow!("Could not determine home directory"))
}

/// Resolve `~/.leviath/agents`, the directory `lev list` scans for installed
/// agents.
///
/// A thin wrapper over [`get_agents_dir_or_error`] supplying the real
/// resolved directory. The `#[cfg(test)]` guard below only lets tests force the
/// "no home directory" error arm of `execute()` deterministically - the real
/// the shared resolver can't be made to return `None` in any environment a
/// test may safely create (on macOS `dirs::home_dir()` falls back to a
/// passwd-database lookup independent of `$HOME`). It does NOT hide the real
/// body from coverage: with the toggle off, `get_agents_dir_or_error(
/// leviath_core::paths::agents_dir())` runs (and is measured) in every ordinary test, and
/// only computes a `PathBuf` (no filesystem writes). The `None` arm of
/// `get_agents_dir_or_error` is covered directly by
/// `get_agents_dir_or_error_none_returns_error`.
fn get_agents_dir() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    if FORCE_AGENTS_DIR_ERROR.with(|f| f.get()) {
        anyhow::bail!("Could not determine home directory");
    }
    get_agents_dir_or_error(leviath_core::paths::agents_dir())
}

#[cfg(test)]
thread_local! {
    /// Test-only toggle letting `execute_returns_err_when_agents_dir_unresolvable`
    /// force `get_agents_dir`'s `Err` arm deterministically.
    static FORCE_AGENTS_DIR_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_test_agent;

    fn write_manifest(dir: &Path, name: &str) {
        write_manifest_with_description(dir, name, "Test agent");
    }

    /// The scanners under a config that grants nothing, which is what every
    /// test predating `[read_paths]` reporting assumed. Tests that care about
    /// grants call the real functions with a config of their own.
    fn read_agent_info(manifest_path: &Path) -> Option<AgentInfo> {
        super::read_agent_info(manifest_path, &Config::default(), Path::new("/work"))
    }

    fn scan_directory_for_agents(dir: &Path) -> Vec<(PathBuf, AgentInfo)> {
        super::scan_directory_for_agents(dir, &Config::default(), Path::new("/work"))
    }

    fn write_manifest_with_description(dir: &Path, name: &str, description: &str) {
        let content = format!(
            r#"[agent]
name = "{}"
version = "1.0.0"
description = "{}"

[stages.main]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Main"
max_iterations = 5

[context.regions]
system = {{ kind = "pinned", max_tokens = 1000 }}
"#,
            name, description
        );
        write_test_agent(dir, content);
    }

    /// An agent that asks to read outside its workdir, for the grant-status
    /// line. Written as an absolute entry so it compiles the same on every OS.
    fn write_read_paths_manifest(dir: &Path, name: &str) {
        let content = format!(
            r#"[agent]
name = "{name}"
version = "1.0.0"
description = "Test agent"

[stages.main]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Main"
max_iterations = 5

[context.regions]
system = {{ kind = "pinned", max_tokens = 1000 }}

[read_paths]
allow = ["/data/runs"]
"#
        );
        write_test_agent(dir, content);
    }

    fn info_with_config(dir: &Path, config: &Config) -> AgentInfo {
        super::read_agent_info(&dir.join("agent.leviath"), config, Path::new("/work"))
            .expect("manifest parses")
    }

    /// The reported bug, in the listing: an agent whose declarations nothing
    /// grants must say so, and point at where the detail is.
    #[test]
    fn an_ungranted_read_paths_declaration_is_listed_as_such() {
        let dir = tempfile::tempdir().unwrap();
        write_read_paths_manifest(dir.path(), "cto");
        let summary = info_with_config(dir.path(), &Config::default())
            .read_paths
            .expect("declares read paths");
        assert!(summary.contains("1 declared, 0 granted"), "{summary}");
        assert!(summary.contains("lev validate"), "{summary}");
    }

    #[test]
    fn a_granted_read_paths_declaration_needs_no_pointer() {
        let dir = tempfile::tempdir().unwrap();
        write_read_paths_manifest(dir.path(), "cto");
        let mut config = Config::default();
        config.security.read_paths = vec!["/data/runs".to_string()];
        let summary = info_with_config(dir.path(), &config)
            .read_paths
            .expect("declares read paths");
        assert_eq!(summary, "read_paths: 1 declared, 1 granted");
    }

    /// A grant list that cannot compile is a hard spawn error later; saying so
    /// here beats printing a count derived from nothing.
    #[test]
    fn a_broken_grant_list_is_reported_on_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        write_read_paths_manifest(dir.path(), "cto");
        let mut config = Config::default();
        config.security.read_paths = vec!["regex:relative/.*".to_string()];
        let summary = info_with_config(dir.path(), &config)
            .read_paths
            .expect("declares read paths");
        assert!(summary.contains("config.toml"), "{summary}");
    }

    #[test]
    fn an_agent_declaring_no_read_paths_gets_no_line() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "plain");
        assert!(
            info_with_config(dir.path(), &Config::default())
                .read_paths
                .is_none()
        );
    }

    #[test]
    fn read_agent_info_valid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "my-agent");
        let info = read_agent_info(&dir.path().join("agent.leviath")).unwrap();
        assert_eq!(info.name, "my-agent");
        assert_eq!(info.version, "1.0.0");
        assert_eq!(info.description, "Test agent");
    }

    #[test]
    fn read_agent_info_missing_file_returns_none() {
        let result = read_agent_info(Path::new("/nonexistent/agent.leviath"));
        assert!(result.is_none());
    }

    #[test]
    fn read_agent_info_invalid_toml_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("agent.leviath"), "not valid toml {{{{").unwrap();
        let result = read_agent_info(&dir.path().join("agent.leviath"));
        assert!(result.is_none());
    }

    #[test]
    fn scan_directory_nonexistent_returns_empty() {
        let agents = scan_directory_for_agents(Path::new("/nonexistent/path"));
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_directory_path_is_a_file_returns_empty() {
        // `dir.exists()` is true for a plain file too, so this reaches
        // `fs::read_dir(dir)` - which fails with "not a directory",
        // exercising the `if let Ok(entries) = ...` construct's implicit
        // (no-`else`) false arm that no other test hits.
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("not-a-directory.txt");
        fs::write(&file_path, "hello").unwrap();
        let agents = scan_directory_for_agents(&file_path);
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_directory_direct_manifest_invalid_is_skipped() {
        // The direct-manifest branch (as opposed to the subdirectory-scan
        // branch, covered separately by `scan_directory_subdir_with_invalid_manifest`)
        // has its own `if let Some(info) = read_agent_info(...)` - this
        // exercises that branch's `None` arm when the manifest at the
        // directory's own root is present but unparseable.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("agent.leviath"), "not valid toml {{{{").unwrap();
        let agents = scan_directory_for_agents(dir.path());
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_directory_with_direct_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "direct-agent");
        let agents = scan_directory_for_agents(dir.path());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].1.name, "direct-agent");
    }

    #[test]
    fn scan_directory_with_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let sub1 = dir.path().join("agent-a");
        let sub2 = dir.path().join("agent-b");
        fs::create_dir_all(&sub1).unwrap();
        fs::create_dir_all(&sub2).unwrap();
        write_manifest(&sub1, "agent-a");
        write_manifest(&sub2, "agent-b");

        let agents = scan_directory_for_agents(dir.path());
        assert_eq!(agents.len(), 2);
        let names: Vec<&str> = agents.iter().map(|a| a.1.name.as_str()).collect();
        assert!(names.contains(&"agent-a"));
        assert!(names.contains(&"agent-b"));
    }

    #[test]
    fn scan_directory_ignores_subdirs_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("no-manifest");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("readme.txt"), "not a manifest").unwrap();

        let agents = scan_directory_for_agents(dir.path());
        assert!(agents.is_empty());
    }

    // ─── --filter actually filters ──────────────────────────────────────

    /// The report a filter produces, from a directory holding one agent.
    fn report_under(filter: ListFilter) -> ListReport {
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let sub = agents_dir.path().join("installed-agent");
        fs::create_dir_all(&sub).unwrap();
        write_manifest(&sub, "installed-agent");
        build_list_report(agents_dir.path(), cwd.path(), &Config::default(), filter)
    }

    #[test]
    fn filter_agents_reports_agents_and_no_blueprints() {
        // The flag parsed and was then never read, so all three spellings
        // printed the same thing. Assert the halves are actually separable.
        let report = report_under(ListFilter::Agents);
        assert!(!report.agents.is_empty(), "runnable agents are the point");
        assert!(report.bundled.is_empty(), "blueprints were not asked for");
    }

    #[test]
    fn filter_blueprints_reports_blueprints_and_no_agents() {
        let report = report_under(ListFilter::Blueprints);
        assert!(report.agents.is_empty(), "agents were not asked for");
        assert!(!report.bundled.is_empty(), "the catalog is never empty");
    }

    #[test]
    fn filter_all_reports_both() {
        let report = report_under(ListFilter::All);
        assert!(!report.agents.is_empty());
        assert!(!report.bundled.is_empty());
    }

    /// An excluded half is an empty list, not a missing key, so a `--json`
    /// consumer indexes the same shape whatever the filter was.
    #[test]
    fn an_excluded_half_is_present_and_empty_in_json() {
        let json = serde_json::to_value(report_under(ListFilter::Agents)).unwrap();
        assert!(
            json.get("bundled")
                .is_some_and(|b| b.as_array().is_some_and(Vec::is_empty))
        );
        let json = serde_json::to_value(report_under(ListFilter::Blueprints)).unwrap();
        assert!(
            json.get("agents")
                .is_some_and(|a| a.as_array().is_some_and(Vec::is_empty))
        );
    }

    /// clap rejects a spelling it does not know. The old `String` accepted
    /// anything and read none of it, so a typo silently printed everything.
    #[test]
    fn an_unknown_filter_is_refused() {
        use clap::Parser;

        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: ListArgs,
        }

        assert!(Cli::try_parse_from(["lev", "--filter", "agants"]).is_err());
        let ok = Cli::try_parse_from(["lev", "--filter", "agents"]).expect("a known spelling");
        assert_eq!(ok.args.filter, ListFilter::Agents);
    }

    #[test]
    fn list_args_default_filter() {
        let args = ListArgs {
            filter: ListFilter::All,
            json: false,
        };
        assert_eq!(args.filter, ListFilter::All);
    }

    // ─── read_agent_info: description and version ───────────────────────

    #[test]
    fn read_agent_info_extracts_description() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "my-agent");
        let info = read_agent_info(&dir.path().join("agent.leviath")).unwrap();
        assert_eq!(info.description, "Test agent");
        assert_eq!(info.version, "1.0.0");
    }

    // ─── scan_directory: nested but not deep ────────────────────────────

    #[test]
    fn scan_directory_with_both_direct_and_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        // Direct manifest
        write_manifest(dir.path(), "root-agent");
        // Subdirectory with manifest
        let sub = dir.path().join("child");
        fs::create_dir_all(&sub).unwrap();
        write_manifest(&sub, "child-agent");

        let agents = scan_directory_for_agents(dir.path());
        assert_eq!(agents.len(), 2);
        let names: Vec<&str> = agents.iter().map(|a| a.1.name.as_str()).collect();
        assert!(names.contains(&"root-agent"));
        assert!(names.contains(&"child-agent"));
    }

    // ─── scan_directory: empty directory ────────────────────────────────

    #[test]
    fn scan_directory_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let agents = scan_directory_for_agents(dir.path());
        assert!(agents.is_empty());
    }

    // ─── scan_directory: subdirectory with invalid manifest ─────────────

    #[test]
    fn scan_directory_subdir_with_invalid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("bad-agent");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("agent.leviath"), "invalid toml {{{{").unwrap();

        let agents = scan_directory_for_agents(dir.path());
        assert!(agents.is_empty());
    }

    // ─── get_agents_dir ────────────────────────────────────────────────

    #[test]
    fn get_agents_dir_returns_path_with_agents() {
        let dir = get_agents_dir().unwrap();
        assert!(dir.to_str().unwrap().contains(".leviath"));
        assert!(dir.to_str().unwrap().ends_with("agents"));
    }

    #[test]
    fn get_agents_dir_or_error_some_returns_path() {
        let dir = PathBuf::from("/home/testuser/.leviath/agents");
        assert_eq!(get_agents_dir_or_error(Some(dir.clone())).unwrap(), dir);
    }

    #[test]
    fn get_agents_dir_or_error_none_returns_error() {
        let err = get_agents_dir_or_error(None).unwrap_err();
        assert!(
            err.to_string()
                .contains("Could not determine home directory")
        );
    }

    // ─── read_agent_info: minimal manifest ──────────────────────────────

    #[test]
    fn read_agent_info_minimal_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let content = r#"[agent]
name = "minimal"
version = "0.0.1"
description = ""

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        write_test_agent(dir.path(), content);
        let info = read_agent_info(&dir.path().join("agent.leviath")).unwrap();
        assert_eq!(info.name, "minimal");
        assert_eq!(info.description, "");
    }

    // ─── execute() smoke test (real environment) ────────────────────────

    #[tokio::test]
    async fn execute_runs_without_error() {
        // Isolated: this reaches `Config::load()`, which reads process-wide
        // environment. Unisolated it races every `temp_env` test in the binary.
        crate::config::with_isolated_config_path_async("list-runs-ok", |_fake_dir| async move {
            // Touches the real environment (home dir / CWD / exe location /
            // config) but must always succeed regardless of what it finds.
            let args = ListArgs {
                filter: ListFilter::All,
                json: false,
            };
            let result = execute(args).await;
            assert!(result.is_ok());
        })
        .await;
    }

    #[tokio::test]
    async fn execute_returns_err_when_agents_dir_unresolvable() {
        // Isolated: this reaches `Config::load()`, which reads process-wide
        // environment. Unisolated it races every `temp_env` test in the binary.
        crate::config::with_isolated_config_path_async("list-dir-err", |_fake_dir| async move {
            // Drives `execute`'s `get_agents_dir()?` error-propagation branch
            // for real via the test-only `FORCE_AGENTS_DIR_ERROR` toggle on
            // `get_agents_dir`'s twin (see its doc comment for why the real
            // implementation's failure can't be forced directly).
            FORCE_AGENTS_DIR_ERROR.with(|f| f.set(true));
            let args = ListArgs {
                filter: ListFilter::All,
                json: false,
            };
            let result = execute(args).await;
            FORCE_AGENTS_DIR_ERROR.with(|f| f.set(false));

            let err = result.unwrap_err();
            assert!(
                err.to_string()
                    .contains("Could not determine home directory")
            );
        })
        .await;
    }

    // `execute`'s `std::env::current_dir().unwrap_or_default()` can only take
    // its `Err` arm in a real (if rare) TOCTOU scenario: the process's CWD is
    // removed out from under it. That's genuinely reproducible on Unix (not a
    // fake): create a directory, `chdir` into it, then delete it --
    // `current_dir()` then reliably returns an error. On Windows this same
    // sequence isn't reproducible: NTFS/Win32 refuse to remove a directory
    // that's a live process's current working directory (a sharing
    // violation), so `remove_dir_all` itself fails there instead of
    // succeeding - confirmed via real Windows CI. Unix-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn execute_falls_back_to_default_cwd_when_current_dir_is_gone() {
        // Isolated: this reaches `Config::load()`, which reads process-wide
        // environment. Unisolated it races every `temp_env` test in the binary.
        crate::config::with_isolated_config_path_async("list-cwd-gone", |_fake_dir| async move {
            // `isolate_cwd_for_test` serializes against every other CWD-mutating
            // test in the crate and restores CWD automatically on drop, so it's
            // safe to hold across the `.await` below.
            let _guard = crate::config::isolate_cwd_for_test();
            let dir = tempfile::tempdir().unwrap();
            std::env::set_current_dir(dir.path()).unwrap();
            std::fs::remove_dir_all(dir.path()).unwrap();

            let args = ListArgs {
                filter: ListFilter::All,
                json: false,
            };
            let result = execute(args).await;

            assert!(result.is_ok());
        })
        .await;
    }

    /// Cross-platform companion to the Unix-only real-filesystem test above:
    /// forces [`resolve_cwd`]'s `Err` arm deterministically via
    /// [`super::super::force_cwd_error`] so `execute`'s `unwrap_or_default()` fallback is
    /// also exercised on Windows, where the real filesystem race isn't
    /// reproducible.
    #[tokio::test]
    async fn execute_falls_back_to_default_cwd_via_forced_error() {
        // Isolated: this reaches `Config::load()`, which reads process-wide
        // environment. Unisolated it races every `temp_env` test in the binary.
        crate::config::with_isolated_config_path_async("list-cwd-forced", |_fake_dir| async move {
            crate::commands::force_cwd_error(true);
            let args = ListArgs {
                filter: ListFilter::All,
                json: false,
            };
            let result = execute(args).await;
            crate::commands::force_cwd_error(false);

            assert!(result.is_ok());
        })
        .await;
    }

    /// A config that exists but doesn't parse must fail the command, not
    /// silently list from the default `agent_paths` (regression: this used to
    /// be `unwrap_or_default()`, which hid the user's agent directories with
    /// no hint why).
    #[tokio::test]
    async fn execute_fails_loudly_on_a_broken_config() {
        crate::config::with_isolated_config_path_async(
            "list-broken-config",
            |fake_dir| async move {
                std::fs::write(fake_dir.join("config.toml"), "not = valid = toml").unwrap();
                let args = ListArgs {
                    filter: ListFilter::All,
                    json: false,
                };
                let err = execute(args).await.expect_err("broken config must error");
                assert!(err.to_string().contains("parse"), "{err}");
            },
        )
        .await;
    }

    // ─── print_agent_listing (fully injectable) ─────────────────────────

    // ─── --json ──────────────────────────────────────────────────────────

    #[test]
    fn json_listing_tags_each_agent_with_where_it_came_from() {
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let configured = tempfile::tempdir().unwrap();

        let installed = agents_dir.path().join("from-install");
        fs::create_dir_all(&installed).unwrap();
        write_manifest(&installed, "installed-agent");
        write_manifest(cwd.path(), "local-agent");
        let extra = configured.path().join("from-config");
        fs::create_dir_all(&extra).unwrap();
        write_manifest(&extra, "configured-agent");

        let config = Config {
            agent_paths: vec![configured.path().to_path_buf()],
            ..Config::default()
        };
        let report = build_list_report(agents_dir.path(), cwd.path(), &config, ListFilter::All);

        let sourced: Vec<(&str, &str)> = report
            .agents
            .iter()
            .map(|a| (a.info.name.as_str(), a.source))
            .collect();
        assert!(sourced.contains(&("installed-agent", "installed")));
        assert!(sourced.contains(&("configured-agent", "configured")));
        assert!(sourced.contains(&("local-agent", "local")));
    }

    #[test]
    fn json_listing_reports_the_bundled_catalog_separately_from_runnable_agents() {
        // Bundled agents are not runnable until installed, so they must not
        // appear in `agents` on a machine with nothing installed.
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();

        let report = build_list_report(
            agents_dir.path(),
            cwd.path(),
            &Config::default(),
            ListFilter::All,
        );
        assert!(report.agents.is_empty());
        assert_eq!(report.bundled.len(), crate::bundled::BUNDLED_AGENTS.len());
    }

    #[test]
    fn json_listing_flattens_the_agent_fields_next_to_its_source() {
        // `#[serde(flatten)]` is easy to lose in a refactor, and losing it would
        // nest every agent under an `info` key that no caller expects.
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_manifest(cwd.path(), "flat-agent");

        let report = build_list_report(
            agents_dir.path(),
            cwd.path(),
            &Config::default(),
            ListFilter::All,
        );
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(value["agents"][0]["name"], serde_json::json!("flat-agent"));
        assert_eq!(value["agents"][0]["source"], serde_json::json!("local"));
        assert!(value["agents"][0]["path"].is_string());
    }

    #[tokio::test]
    async fn execute_with_json_runs_without_error() {
        crate::config::with_isolated_config_path_async("list-json-ok", |_fake_dir| async move {
            let args = ListArgs {
                filter: ListFilter::All,
                json: true,
            };
            assert!(execute(args).await.is_ok());
        })
        .await;
    }

    #[test]
    fn print_agent_listing_nothing_installed() {
        // The bundled catalog is always non-empty, so it must not count as
        // "you have agents" - otherwise the get-started guidance would be
        // suppressed for exactly the fresh install that needs it.
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let config = Config::default();

        let result = print_agent_listing(
            agents_dir.path(),
            cwd.path(),
            None,
            &config,
            ListFilter::All,
        );
        assert!(result.is_ok());
    }

    /// The prose report honours the filter too, including its configured-paths
    /// scan and the get-started guidance, which would be a non sequitur under
    /// `--filter blueprints`.
    #[test]
    fn print_agent_listing_honours_each_filter() {
        let agents_dir = tempfile::tempdir().unwrap();
        let sub = agents_dir.path().join("installed-agent");
        fs::create_dir_all(&sub).unwrap();
        write_manifest(&sub, "installed-agent");
        let cwd = tempfile::tempdir().unwrap();

        // A configured path with an agent in it, so section 3 is non-empty and
        // its skip-under-filter arm is a real choice rather than a no-op.
        let configured = tempfile::tempdir().unwrap();
        let other = configured.path().join("configured-agent");
        fs::create_dir_all(&other).unwrap();
        write_manifest(&other, "configured-agent");
        let config = Config {
            agent_paths: vec![configured.path().to_path_buf()],
            ..Config::default()
        };

        for filter in [ListFilter::All, ListFilter::Agents, ListFilter::Blueprints] {
            let result = print_agent_listing(agents_dir.path(), cwd.path(), None, &config, filter);
            assert!(result.is_ok(), "{filter:?}");
        }
    }

    /// The bundled section's second source is the on-disk `<exe_dir>/agents`
    /// scan, which must also be skipped when blueprints were not asked for.
    #[test]
    fn print_agent_listing_skips_the_exe_dir_scan_for_agents_only() {
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let exe_dir = tempfile::tempdir().unwrap();
        let bundled = exe_dir.path().join("agents").join("side-loaded");
        fs::create_dir_all(&bundled).unwrap();
        write_manifest(&bundled, "side-loaded");

        for filter in [ListFilter::All, ListFilter::Agents, ListFilter::Blueprints] {
            let result = print_agent_listing(
                agents_dir.path(),
                cwd.path(),
                Some(exe_dir.path()),
                &Config::default(),
                filter,
            );
            assert!(result.is_ok(), "{filter:?}");
        }
    }

    #[test]
    fn print_agent_listing_finds_installed_agent() {
        let agents_dir = tempfile::tempdir().unwrap();
        let sub = agents_dir.path().join("installed-agent");
        fs::create_dir_all(&sub).unwrap();
        write_manifest(&sub, "installed-agent");

        let cwd = tempfile::tempdir().unwrap();
        let config = Config::default();

        let result = print_agent_listing(
            agents_dir.path(),
            cwd.path(),
            None,
            &config,
            ListFilter::All,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn print_agent_listing_finds_local_manifest() {
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_manifest(cwd.path(), "local-agent");
        let config = Config::default();

        let result = print_agent_listing(
            agents_dir.path(),
            cwd.path(),
            None,
            &config,
            ListFilter::All,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn print_agent_listing_local_manifest_invalid_is_skipped() {
        // The local-manifest section has its own `if let Some(info) = ...`
        // construct with no `else`; this exercises its false arm (an
        // existing but unparseable `agent.leviath` in the cwd), which
        // `print_agent_listing_finds_local_manifest` (valid manifest) never
        // reaches.
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        fs::write(cwd.path().join("agent.leviath"), "not valid toml {{{{").unwrap();
        let config = Config::default();

        let result = print_agent_listing(
            agents_dir.path(),
            cwd.path(),
            None,
            &config,
            ListFilter::All,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn print_agent_listing_finds_configured_path_agent() {
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let configured = tempfile::tempdir().unwrap();
        let sub = configured.path().join("configured-agent");
        fs::create_dir_all(&sub).unwrap();
        write_manifest(&sub, "configured-agent");

        let config = Config {
            agent_paths: vec![configured.path().to_path_buf()],
            ..Config::default()
        };

        let result = print_agent_listing(
            agents_dir.path(),
            cwd.path(),
            None,
            &config,
            ListFilter::All,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn print_agent_listing_finds_builtin_agents() {
        // An `agents/` directory beside the executable contributes a blueprint
        // the embedded catalog doesn't have, so it is appended to the list.
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let exe_dir = tempfile::tempdir().unwrap();
        let builtin_dir = exe_dir.path().join("agents");
        let sub = builtin_dir.join("builtin-agent");
        fs::create_dir_all(&sub).unwrap();
        write_manifest(&sub, "builtin-agent");
        let config = Config::default();

        let result = print_agent_listing(
            agents_dir.path(),
            cwd.path(),
            Some(exe_dir.path()),
            &config,
            ListFilter::All,
        );
        assert!(result.is_ok());
    }

    /// The listing prints the `[read_paths]` line for an agent that has one,
    /// which is the second line `print_agent` can emit.
    #[test]
    fn print_agent_listing_carries_the_read_paths_line() {
        let agents_dir = tempfile::tempdir().unwrap();
        let agent = agents_dir.path().join("cto");
        fs::create_dir_all(&agent).unwrap();
        write_read_paths_manifest(&agent, "cto");
        let cwd = tempfile::tempdir().unwrap();

        let result = print_agent_listing(
            agents_dir.path(),
            cwd.path(),
            None,
            &Config::default(),
            ListFilter::All,
        );

        assert!(result.is_ok());
        // The line itself is asserted where it is built, without capturing
        // stdout; this is the path that reaches the printer with one to print.
        assert!(
            info_with_config(&agent, &Config::default())
                .read_paths
                .is_some()
        );
    }

    #[test]
    fn print_agent_listing_does_not_list_a_bundled_agent_twice() {
        // Running from a git checkout puts the *same* blueprints both in the
        // embedded catalog and in `<exe_dir>/agents`. Listing each one twice
        // would be pure noise, so the on-disk scan only appends what the
        // catalog doesn't already carry.
        let bundled = &crate::bundled::BUNDLED_AGENTS[0];
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let exe_dir = tempfile::tempdir().unwrap();
        let sub = exe_dir.path().join("agents").join(bundled.name);
        fs::create_dir_all(&sub).unwrap();
        crate::bundled::install_bundled(bundled, &exe_dir.path().join("agents")).unwrap();
        let config = Config::default();

        let result = print_agent_listing(
            agents_dir.path(),
            cwd.path(),
            Some(exe_dir.path()),
            &config,
            ListFilter::All,
        );

        assert!(result.is_ok());
        // The same name+version pair the catalog already holds resolves to one
        // entry, not two.
        let entry = format!("{} (v{})", bundled.name, bundled.version);
        let names: Vec<String> = crate::bundled::BUNDLED_AGENTS
            .iter()
            .map(|a| format!("{} (v{})", a.name, a.version))
            .collect();
        assert_eq!(names.iter().filter(|n| **n == entry).count(), 1);
    }

    #[test]
    fn print_agent_listing_all_sources_populated() {
        let agents_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(agents_dir.path().join("installed")).unwrap();
        write_manifest(&agents_dir.path().join("installed"), "installed");

        let cwd = tempfile::tempdir().unwrap();
        write_manifest(cwd.path(), "local");

        let configured = tempfile::tempdir().unwrap();
        fs::create_dir_all(configured.path().join("configured")).unwrap();
        write_manifest(&configured.path().join("configured"), "configured");

        let exe_dir = tempfile::tempdir().unwrap();
        let builtin_sub = exe_dir.path().join("agents").join("builtin");
        fs::create_dir_all(&builtin_sub).unwrap();
        write_manifest(&builtin_sub, "builtin");

        let config = Config {
            agent_paths: vec![configured.path().to_path_buf()],
            ..Config::default()
        };

        let result = print_agent_listing(
            agents_dir.path(),
            cwd.path(),
            Some(exe_dir.path()),
            &config,
            ListFilter::All,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn print_agent_listing_empty_descriptions_across_all_sources() {
        // Every section (installed / local / configured-path) has its own
        // "empty description -> no dash suffix" branch; the tests above only
        // ever exercise the non-empty path for all three, since
        // `write_manifest` hardcodes a non-empty description.
        let agents_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(agents_dir.path().join("installed")).unwrap();
        write_manifest_with_description(&agents_dir.path().join("installed"), "installed", "");

        let cwd = tempfile::tempdir().unwrap();
        write_manifest_with_description(cwd.path(), "local", "");

        let configured = tempfile::tempdir().unwrap();
        fs::create_dir_all(configured.path().join("configured")).unwrap();
        write_manifest_with_description(&configured.path().join("configured"), "configured", "");

        let config = Config {
            agent_paths: vec![configured.path().to_path_buf()],
            ..Config::default()
        };

        let result = print_agent_listing(
            agents_dir.path(),
            cwd.path(),
            None,
            &config,
            ListFilter::All,
        );
        assert!(result.is_ok());
    }

    // ─── scan_directory: agent with empty description ────────────────────

    #[test]
    fn scan_directory_agent_with_empty_description() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("my-agent");
        fs::create_dir_all(&sub).unwrap();
        let content = r#"[agent]
name = "my-agent"
version = "2.0.0"
description = ""

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        write_test_agent(sub, content);

        let agents = scan_directory_for_agents(dir.path());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].1.description, "");
    }

    // ─── scan_directory: multiple subdirs with mixed manifests ──────────

    #[test]
    fn scan_directory_mixed_valid_and_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good");
        let bad = dir.path().join("bad");
        let empty = dir.path().join("empty");
        fs::create_dir_all(&good).unwrap();
        fs::create_dir_all(&bad).unwrap();
        fs::create_dir_all(&empty).unwrap();

        write_manifest(&good, "good-agent");
        fs::write(bad.join("agent.leviath"), "bad {{ toml").unwrap();

        let agents = scan_directory_for_agents(dir.path());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].1.name, "good-agent");
    }
}
