//! The agent blueprints shipped inside the `lev` binary, and the planner that
//! decides what to do with them.
//!
//! Embedding is what makes the ten blueprints under the workspace's `agents/`
//! directory reachable outside a git checkout: `lev add` takes a local path,
//! and an `agents/` directory next to the executable is a layout no real
//! install has, so without the bundle a user who downloads a release binary
//! gets a working runtime and zero agents to run on it.
//!
//! `build.rs` embeds every file of every blueprint via `include_str!` (23
//! files, ~170 KB of text) and generates the [`BUNDLED_AGENTS`] table included
//! below. `lev setup` offers to install them; `lev list` reports them.

include!(concat!(env!("OUT_DIR"), "/bundled_agents.rs"));

use std::path::Path;

/// What `lev setup` should do with one bundled blueprint, given what is
/// currently installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
    /// Not installed.
    Install,
    /// Installed at a different version.
    Update { from: String },
    /// Installed at the bundled version.
    UpToDate,
}

impl AgentAction {
    /// Whether applying this action would change anything on disk. Drives which
    /// rows the wizard pre-checks.
    pub fn is_change(&self) -> bool {
        !matches!(self, Self::UpToDate)
    }

    /// Short label for the wizard's blueprint list.
    pub fn label(&self, to: &str) -> String {
        match self {
            Self::Install => format!("install {to}"),
            Self::Update { from } => format!("update {from} → {to}"),
            Self::UpToDate => "up to date".to_string(),
        }
    }
}

/// The installed version of `name` under `agents_dir`, if a readable manifest
/// is there.
///
/// Deliberately lenient: a blueprint directory whose manifest is missing or
/// unparseable reads as *not installed*, so the wizard offers a clean reinstall
/// instead of refusing to plan. An unreadable manifest is exactly the state a
/// half-finished copy leaves behind.
pub fn installed_version(agents_dir: &Path, name: &str) -> Option<String> {
    let manifest = std::fs::read_to_string(agents_dir.join(name).join("agent.leviath")).ok()?;
    leviath_core::manifest::parse_manifest(&manifest)
        .ok()
        .map(|bp| bp.version)
}

/// Decide what to do with every bundled blueprint.
///
/// Version comparison is plain string inequality, not semver ordering: this
/// crate has no semver dependency, and both versions are shown to the user
/// anyway, so a hand-edited blueprint surfaces as an offered update they can
/// decline rather than being silently overwritten or silently skipped.
///
/// Known limitation: a blueprint edited *without* bumping its version reads as
/// up to date, because nothing hashes the contents.
pub fn plan_agent_actions(agents_dir: &Path) -> Vec<(&'static BundledAgent, AgentAction)> {
    BUNDLED_AGENTS
        .iter()
        .map(|agent| {
            let action = match installed_version(agents_dir, agent.name) {
                None => AgentAction::Install,
                Some(v) if v == agent.version => AgentAction::UpToDate,
                Some(from) => AgentAction::Update { from },
            };
            (agent, action)
        })
        .collect()
}

/// Write one bundled blueprint into `<agents_dir>/<name>/`, replacing whatever
/// is there.
///
/// The existing tree is removed first rather than merged over: a stale file
/// from an older version of the blueprint (a tool script that was dropped, say)
/// would otherwise survive forever and keep being loaded. This mirrors what
/// `lev add`'s directory install already does.
pub fn install_bundled(agent: &BundledAgent, agents_dir: &Path) -> anyhow::Result<()> {
    let dest = agents_dir.join(agent.name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    for (rel, contents) in agent.files {
        // Derive the parent from the *relative* path rather than calling
        // `path.parent()`. `dest.join(rel)` always has a parent, so the `None`
        // arm of `parent()` would be unreachable code pretending to be a
        // handled case; splitting `rel` gives two arms that both actually
        // happen - nested (`tools/web_fetch.rhai`) and flat (`agent.leviath`).
        let parent = match rel.rsplit_once('/') {
            Some((dir, _)) => dest.join(dir),
            None => dest.clone(),
        };
        std::fs::create_dir_all(&parent)?;
        std::fs::write(dest.join(rel), contents)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every assertion here is an invariant over *all* discovered blueprints.
    /// Naming individual agents would turn adding or renaming one into a test
    /// edit, and would stop testing the property the moment the list drifted.
    #[test]
    fn every_bundled_agent_has_a_name_version_and_manifest() {
        assert!(
            !BUNDLED_AGENTS.is_empty(),
            "the binary shipped with no blueprints -- build.rs found no agents/ directory"
        );
        for agent in BUNDLED_AGENTS {
            assert!(!agent.name.is_empty(), "a bundled agent has an empty name");
            assert!(
                !agent.version.is_empty(),
                "bundled agent {} has an empty version",
                agent.name
            );
            assert!(
                agent.files.iter().any(|(rel, _)| *rel == "agent.leviath"),
                "bundled agent {} has no agent.leviath",
                agent.name
            );
            for (rel, contents) in agent.files {
                assert!(
                    !rel.is_empty(),
                    "bundled agent {} has an empty path",
                    agent.name
                );
                assert!(
                    !contents.is_empty(),
                    "bundled agent {} has an empty file {rel}",
                    agent.name
                );
            }
        }
    }

    /// A tool script shipped under the same filename by more than one agent
    /// must be byte-identical everywhere.
    ///
    /// Each agent directory is self-contained - that is what lets `lev add
    /// <dir>` and `lev pack` work - so `web_fetch.rhai` and `web_search.rhai`
    /// exist as five copies each rather than one shared file. That is fine
    /// until one copy is fixed and the others are not: these scripts are the
    /// agents' network surface, so a hardening change applied to one of five is
    /// four agents still carrying the unfixed behaviour, with nothing to say so.
    ///
    /// This turns that silent drift into a test failure. Deliberately keyed on
    /// filename over *all* discovered agents rather than naming the five, so it
    /// keeps holding as agents are added or renamed.
    #[test]
    fn a_tool_script_shared_by_several_agents_is_identical_in_all_of_them() {
        use std::collections::HashMap;

        // filename -> (first agent that shipped it, its contents)
        let mut first_seen: HashMap<&str, (&str, &str)> = HashMap::new();
        for agent in BUNDLED_AGENTS {
            for (rel, contents) in agent.files {
                let Some(filename) = rel.strip_prefix("tools/") else {
                    continue;
                };
                match first_seen.get(filename) {
                    Some((other, expected)) => assert!(
                        expected == contents,
                        "tools/{filename} differs between bundled agents {other} and {} - \
                         a change to one copy was not applied to the others",
                        agent.name
                    ),
                    None => {
                        first_seen.insert(filename, (agent.name, contents));
                    }
                }
            }
        }
        // Guard against a vacuous pass: if the scan found no tool scripts at
        // all, the loop above asserts nothing.
        assert!(
            !first_seen.is_empty(),
            "no bundled agent ships a tools/ script - this invariant is not being tested"
        );
    }

    #[test]
    fn every_bundled_manifest_parses_and_agrees_with_its_recorded_version() {
        // The recorded version drives install/update planning, so a build.rs
        // scan that disagreed with the manifest would make the wizard lie.
        for agent in BUNDLED_AGENTS {
            let manifest = agent
                .files
                .iter()
                .find(|(rel, _)| *rel == "agent.leviath")
                .map(|(_, c)| *c)
                .expect("checked above");
            // `.expect`, not `.unwrap_or_else(|e| panic!(...))`: the closure in
            // the latter is a function that never runs on a passing test, which
            // reads to llvm-cov as an uncovered region. For the same reason the
            // message is a literal - a *call* in an `assert!`'s format args is
            // also a region that only the failing path reaches.
            let parsed = leviath_core::manifest::parse_manifest(manifest);
            assert!(
                parsed.is_ok(),
                "bundled agent {} does not parse",
                agent.name
            );
            let blueprint = parsed.expect("asserted Ok just above");
            assert_eq!(blueprint.version, agent.version);
            assert_eq!(blueprint.name, agent.name);
        }
    }

    /// Every provider `lev setup` can configure. Claude Code is a transport
    /// rather than a provider a stage names, so it is not in this list.
    const SETUP_PROVIDERS: &[&str] = &["anthropic", "openai", "google", "openrouter", "ollama"];

    #[test]
    fn every_bundled_stage_offers_every_provider_setup_can_configure() {
        // Getting Started promises that one provider is all you need. That is
        // only true if each stage lists them all: a stage naming a subset fails
        // at spawn on a machine holding a key for a provider it left out.
        //
        // Discovered from BUNDLED_AGENTS rather than enumerated, so a new
        // blueprint is covered the day it lands.
        for agent in BUNDLED_AGENTS {
            let manifest = agent
                .files
                .iter()
                .find(|(rel, _)| *rel == "agent.leviath")
                .map(|(_, c)| *c)
                .expect("every bundled agent has a manifest");
            let blueprint =
                leviath_core::manifest::parse_manifest(manifest).expect("manifest parses");

            for stage in &blueprint.stages {
                let stage_name = &stage.name;
                let listed: Vec<&str> = stage
                    .model
                    .models
                    .iter()
                    .map(|entry| entry.provider.as_str())
                    .collect();
                for provider in SETUP_PROVIDERS {
                    assert!(
                        listed.contains(provider),
                        "{}/{} omits provider {}",
                        agent.name,
                        stage_name,
                        provider
                    );
                }
                // Ollama needs no API key, so it registers on every machine. Any
                // position but last makes it beat a provider the user actually
                // configured, and the run then dies on its first inference.
                assert_eq!(
                    listed.last().copied(),
                    Some("ollama"),
                    "{}/{} must list ollama last",
                    agent.name,
                    stage_name
                );
            }
        }
    }

    /// The lint env for a bundled agent: the built-ins, the sub-agent tools,
    /// and the agent's own `tools/<name>.rhai`, each of which defines `<name>`.
    ///
    /// Built by hand rather than through `LintEnv::offline`, which discovers
    /// script tools by reading a directory: a bundled agent's files are
    /// compiled into the binary and there is no directory to read.
    fn lint_env_for(agent: &BundledAgent) -> crate::lint::LintEnv {
        let mut known_tools: std::collections::HashSet<String> = leviath_tools::BuiltinTools::new(
            leviath_tools::ToolContext::new(std::path::PathBuf::from(".")),
        )
        .names()
        .into_iter()
        .collect();
        known_tools.extend(leviath_tools::BuiltinTools::subagent_tool_names());
        known_tools.extend(
            agent
                .files
                .iter()
                .filter_map(|(rel, _)| rel.strip_prefix("tools/"))
                .filter_map(|f| f.strip_suffix(".rhai"))
                .map(str::to_string),
        );
        crate::lint::LintEnv {
            known_tools,
            known_models: crate::commands::models::closed_catalog_models(),
            available_providers: None,
            read_paths: None,
        }
    }

    /// No bundled agent ships a blueprint the linter calls broken.
    ///
    /// The errors this catches are the ones that are invisible on inspection: a
    /// tool name matching nothing is silently dropped from what the stage
    /// advertises, so the model is told the tool does not exist and the stage
    /// cannot do its job. A permission for a tool the stage never granted is the
    /// same drift from the other side, reading as a grant and not being one.
    ///
    /// Asserted by running the shipped linter rather than by a parallel copy of
    /// its rules, and over all discovered agents rather than a list of names -
    /// either would stop testing the property the moment it drifted.
    #[test]
    fn no_bundled_agent_has_a_lint_error() {
        for agent in BUNDLED_AGENTS {
            let manifest = agent
                .files
                .iter()
                .find(|(rel, _)| *rel == "agent.leviath")
                .map(|(_, c)| *c)
                .expect("every bundled agent has a manifest");
            let parsed = leviath_core::manifest::parse_manifest(manifest);
            assert!(
                parsed.is_ok(),
                "bundled agent {} does not parse",
                agent.name
            );
            let blueprint = parsed.expect("asserted Ok just above");
            // Every finding is rendered up front, and the errors are then
            // *counted* rather than collected. Any per-error work - a `.map`
            // that formats, a `.collect` into a list of messages - sits in a
            // closure that only runs when the test is about to fail, which
            // llvm-cov reads as an uncovered region for as long as the
            // invariant holds. Counting has no such body.
            let rendered: Vec<(bool, String)> =
                crate::lint::lint_manifest(manifest, &blueprint, &lint_env_for(agent))
                    .iter()
                    .map(|f| (f.is_error(), format!("{} [{}]", f.one_line(), f.code)))
                    .collect();
            let error_count = rendered.iter().filter(|(is_error, _)| *is_error).count();
            assert_eq!(
                error_count, 0,
                "bundled agent {} has lint errors, among {rendered:?}",
                agent.name
            );
        }
    }

    /// The invariant above can actually fail - a check over shipped data that
    /// happens to pass says nothing about whether it would catch drift.
    #[test]
    fn the_lint_invariant_catches_a_typo_and_an_orphan_permission() {
        let manifest = r#"
[agent]
name = "x"
version = "0.1.0"
description = "x"

[stages.only]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-5" }
max_iterations = 5
available_tools = ["read_file", "raed_file"]

[stages.only.tool_permissions]
write_file = "allow"
"#;
        let bp = leviath_core::manifest::parse_manifest(manifest)
            .expect("the fixture parses; it is the lint that should object");
        // Reuse the same env shape a real bundled agent gets, minus any scripts.
        let env = lint_env_for(&BundledAgent {
            name: "x",
            version: "0.1.0",
            files: &[],
        });
        let codes: Vec<&str> = crate::lint::lint_manifest(manifest, &bp, &env)
            .iter()
            .filter(|f| f.is_error())
            .map(|f| f.code)
            .collect();
        assert_eq!(codes, ["unknown-tool", "orphan-stage-permission"]);
    }

    #[test]
    fn bundled_agent_names_are_unique() {
        let mut names: Vec<&str> = BUNDLED_AGENTS.iter().map(|a| a.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(count, names.len(), "duplicate bundled agent names");
    }

    // ─── installed_version ──────────────────────────────────────────────────

    #[test]
    fn installed_version_reads_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let agent = &BUNDLED_AGENTS[0];
        install_bundled(agent, dir.path()).unwrap();

        assert_eq!(
            installed_version(dir.path(), agent.name).as_deref(),
            Some(agent.version)
        );
    }

    #[test]
    fn installed_version_is_none_when_nothing_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(installed_version(dir.path(), "not-installed").is_none());
    }

    #[test]
    fn installed_version_is_none_for_an_unparseable_manifest() {
        // A half-written install must read as "not installed" so the wizard
        // offers a clean reinstall rather than refusing to plan.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("broken")).unwrap();
        std::fs::write(
            dir.path().join("broken/agent.leviath"),
            "not valid toml {{{",
        )
        .unwrap();

        assert!(installed_version(dir.path(), "broken").is_none());
    }

    // ─── plan_agent_actions ─────────────────────────────────────────────────

    #[test]
    fn plan_offers_to_install_everything_into_an_empty_dir() {
        let dir = tempfile::tempdir().unwrap();

        let plan = plan_agent_actions(dir.path());

        assert_eq!(plan.len(), BUNDLED_AGENTS.len());
        for (agent, action) in &plan {
            assert_eq!(*action, AgentAction::Install);
            assert!(action.is_change());
            assert_eq!(
                action.label(agent.version),
                format!("install {}", agent.version)
            );
        }
    }

    #[test]
    fn plan_reports_up_to_date_after_installing() {
        let dir = tempfile::tempdir().unwrap();
        for agent in BUNDLED_AGENTS {
            install_bundled(agent, dir.path()).unwrap();
        }

        let plan = plan_agent_actions(dir.path());

        for (agent, action) in &plan {
            assert_eq!(*action, AgentAction::UpToDate, "{}", agent.name);
            assert!(!action.is_change());
            assert_eq!(action.label(agent.version), "up to date");
        }
    }

    #[test]
    fn plan_reports_an_update_when_the_installed_version_differs() {
        let dir = tempfile::tempdir().unwrap();
        let agent = &BUNDLED_AGENTS[0];
        install_bundled(agent, dir.path()).unwrap();
        // Rewrite the installed manifest at a different version.
        let manifest_path = dir.path().join(agent.name).join("agent.leviath");
        let manifest = std::fs::read_to_string(&manifest_path).unwrap();
        let bumped = manifest.replacen(
            &format!("version = \"{}\"", agent.version),
            "version = \"9.9.9\"",
            1,
        );
        std::fs::write(&manifest_path, bumped).unwrap();

        let plan = plan_agent_actions(dir.path());
        let (_, action) = plan
            .iter()
            .find(|(a, _)| a.name == agent.name)
            .expect("the bundled agent is in the plan");

        assert_eq!(
            *action,
            AgentAction::Update {
                from: "9.9.9".to_string()
            }
        );
        assert!(action.is_change());
        assert_eq!(
            action.label(agent.version),
            format!("update 9.9.9 → {}", agent.version)
        );
    }

    // ─── install_bundled ────────────────────────────────────────────────────

    #[test]
    fn install_writes_every_file_including_nested_ones() {
        let dir = tempfile::tempdir().unwrap();
        // Pick a blueprint that actually has a nested `tools/` file, so the
        // create_dir_all arm is exercised by a real shipped layout rather than
        // a fixture. If none ships nested files any more, the flat arm below
        // still covers the rest.
        for agent in BUNDLED_AGENTS {
            install_bundled(agent, dir.path()).unwrap();
            for (rel, contents) in agent.files {
                let written = std::fs::read_to_string(dir.path().join(agent.name).join(rel));
                assert!(written.is_ok(), "{}/{rel} was not written", agent.name);
                assert_eq!(written.expect("asserted Ok just above"), *contents);
            }
        }
        assert!(
            BUNDLED_AGENTS
                .iter()
                .any(|a| a.files.iter().any(|(rel, _)| rel.contains('/'))),
            "no bundled blueprint has a nested file, so install's mkdir path is untested"
        );
    }

    #[test]
    fn install_replaces_an_existing_tree_and_drops_stale_files() {
        let dir = tempfile::tempdir().unwrap();
        let agent = &BUNDLED_AGENTS[0];
        install_bundled(agent, dir.path()).unwrap();
        let stale = dir
            .path()
            .join(agent.name)
            .join("stale-from-an-older-version");
        std::fs::write(&stale, "leftover").unwrap();

        install_bundled(agent, dir.path()).unwrap();

        assert!(
            !stale.exists(),
            "a reinstall must not leave files from the previous version behind"
        );
        assert!(dir.path().join(agent.name).join("agent.leviath").exists());
    }

    #[test]
    fn install_surfaces_a_directory_creation_failure() {
        // `agents_dir` is itself a file, so creating the blueprint directory
        // under it fails.
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("not-a-dir");
        std::fs::write(&blocked, "").unwrap();

        let result = install_bundled(&BUNDLED_AGENTS[0], &blocked);

        assert!(result.is_err());
    }

    #[test]
    fn install_surfaces_a_file_write_failure() {
        // Isolating the `write` error from the `create_dir_all` error needs a
        // layout where the directory step succeeds and only the write fails.
        // A synthetic blueprint whose second entry names a path the first entry
        // already created as a *directory* does exactly that: `create_dir_all`
        // sees an existing dir and returns Ok, then the write hits EISDIR.
        // No shipped blueprint has that shape, hence the hand-built one.
        let agent = BundledAgent {
            name: "collides-with-its-own-directory",
            version: "0.0.1",
            files: &[("tools/a.rhai", "nested first"), ("tools", "then the dir")],
        };
        let dir = tempfile::tempdir().unwrap();

        let result = install_bundled(&agent, dir.path());

        assert!(result.is_err());
    }

    #[test]
    fn install_surfaces_a_remove_failure() {
        // The destination exists but is a *file*, so `remove_dir_all` fails
        // rather than the write.
        let dir = tempfile::tempdir().unwrap();
        let agent = &BUNDLED_AGENTS[0];
        std::fs::write(dir.path().join(agent.name), "").unwrap();

        let result = install_bundled(agent, dir.path());

        assert!(result.is_err());
    }
}
