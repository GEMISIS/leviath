//! The agent blueprints shipped inside the `lev` binary, and the planner that
//! decides what to do with them.
//!
//! Embedding is what makes the blueprints under the workspace's `agents/`
//! directory reachable outside a git checkout: `lev add` takes a local path,
//! and an `agents/` directory next to the executable is a layout no real
//! install has, so without the bundle a user who downloads a release binary
//! gets a working runtime and zero agents to run on it.
//!
//! `build.rs` embeds every file of every blueprint via `include_str!` and
//! generates the [`BUNDLED_AGENTS`] table included
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
    Update {
        /// The version currently on disk, so the offer can say what it replaces.
        from: String,
    },
    /// Installed at the bundled version, but the files on disk differ from the
    /// bundled ones.
    Modified,
    /// Installed at the bundled version, byte for byte.
    UpToDate,
}

impl AgentAction {
    /// Whether applying this action would change anything on disk.
    pub fn is_change(&self) -> bool {
        !matches!(self, Self::UpToDate)
    }

    /// Whether the wizard should pre-check this row.
    ///
    /// Not the same question as [`Self::is_change`], and the difference is the
    /// point of [`Self::Modified`]: reinstalling over a tree the user edited
    /// destroys their work, and `install_bundled` removes the destination
    /// first, so it destroys files they added too. Offered, never assumed.
    pub fn preselect(&self) -> bool {
        matches!(self, Self::Install | Self::Update { .. })
    }

    /// Short label for the wizard's blueprint list.
    pub fn label(&self, to: &str) -> String {
        match self {
            Self::Install => format!("install {to}"),
            Self::Update { from } => format!("update {from} → {to}"),
            Self::Modified => format!("{to}, edited locally - reinstall overwrites"),
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

/// Whether the installed copy of `agent` is byte-identical to the bundled one.
///
/// [`install_bundled`] removes the destination first, so a tree it wrote has
/// exactly the bundle's files with exactly the bundle's bytes. Any difference -
/// an edited manifest, a tool script the user added, one they deleted - means
/// what is on disk is not what shipped.
///
/// An IO error reads as *differing*, which is the safe direction: the caller
/// uses this to decide whether overwriting is safe, and a directory it cannot
/// read is not one to clobber unasked.
fn matches_bundled(agent: &BundledAgent, agents_dir: &Path) -> bool {
    let dest = agents_dir.join(agent.name);
    for (rel, contents) in agent.files {
        match std::fs::read_to_string(dest.join(rel)) {
            Ok(on_disk) if on_disk == *contents => {}
            _ => return false,
        }
    }
    // Every declared file was found and matched, so equal counts means the two
    // sets are equal - which is what catches a file the user added.
    installed_file_count(&dest) == agent.files.len()
}

/// How many files are under `dir`, recursively.
///
/// An entry that cannot be read counts as one file rather than aborting the
/// walk. The only caller is asking whether the tree is exactly the bundled one,
/// and something on disk it cannot read is already an answer of "no".
fn installed_file_count(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .map(|entry| match entry.map(|e| e.path()) {
            Ok(path) if path.is_dir() => installed_file_count(&path),
            _ => 1,
        })
        .sum()
}

/// Decide what to do with every bundled blueprint.
///
/// Version comparison is plain string inequality, not semver ordering: this
/// crate has no semver dependency, and both versions are shown to the user
/// anyway, so a downgrade and an upgrade both surface as an offered update they
/// can decline.
///
/// A blueprint at the bundled version is only up to date if its files are the
/// bundled files. Comparing versions alone meant a blueprint edited without a
/// version bump read as current forever - and so did a stale install whose
/// version happened to match, which is how an install could sit on an old
/// checkpoint policy while believing itself current. Nothing is hashed and
/// nothing is stored: the bundled bytes are in the binary, so the files
/// themselves are the comparison.
pub fn plan_agent_actions(agents_dir: &Path) -> Vec<(&'static BundledAgent, AgentAction)> {
    BUNDLED_AGENTS
        .iter()
        .map(|agent| {
            let action = match installed_version(agents_dir, agent.name) {
                None => AgentAction::Install,
                Some(v) if v != agent.version => AgentAction::Update { from: v },
                Some(_) if matches_bundled(agent, agents_dir) => AgentAction::UpToDate,
                Some(_) => AgentAction::Modified,
            };
            (agent, action)
        })
        .collect()
}

/// A note for a run about to start on an installed bundled blueprint that this
/// binary ships a different version of.
///
/// `lev setup` is the only thing that has ever said this, and only when asked.
/// Nothing said it at the moment it mattered, so an install could sit versions
/// behind indefinitely - which is exactly how a run kept using an old
/// checkpoint policy while the fix had shipped.
///
/// Deliberately narrow. It fires only for a manifest that *is* the installed
/// copy, under `agents_dir/<name>/`, so a blueprint of the user's own that
/// happens to share a name with a bundled one is never nagged about.
pub fn stale_install_note(
    manifest_path: &Path,
    blueprint: &leviath_core::Blueprint,
    agents_dir: Option<&Path>,
) -> Option<String> {
    let installed = agents_dir?.join(&blueprint.name);
    if !manifest_path.starts_with(&installed) {
        return None;
    }
    let bundled = BUNDLED_AGENTS.iter().find(|a| a.name == blueprint.name)?;
    if bundled.version == blueprint.version {
        return None;
    }
    Some(format!(
        "note: '{}' is installed at {}, and this build ships {}. \
         Run `lev setup` to update it.",
        blueprint.name, blueprint.version, bundled.version
    ))
}

/// Why an installed bundled blueprint would not load, when the reason is that
/// it is old rather than that it is wrong.
///
/// The twin of [`stale_install_note`], for the path where there is no
/// [`leviath_core::Blueprint`] to hand because parsing or validation is what
/// failed. That is exactly when the user most needs to hear it: a graph rule
/// added after their install turns their copy into "invalid blueprint", which
/// reads as a bug in the agent rather than as an out-of-date file, and the
/// version note they would have got on the success path never fires.
///
/// Narrow in the same way: the manifest must *be* the installed copy at
/// `agents_dir/<name>/`, so a blueprint of the user's own is never blamed on a
/// bundled one that shares its name.
pub fn stale_install_hint(manifest_path: &Path, agents_dir: Option<&Path>) -> Option<String> {
    let agents_dir = agents_dir?;
    let bundled = BUNDLED_AGENTS
        .iter()
        .find(|a| manifest_path.starts_with(agents_dir.join(a.name)))?;
    // Content, not the version field. A blueprint's `version` is authored by
    // hand and routinely does not move when the file does, so an install can be
    // months behind while claiming the same number: the two coder blueprints
    // that started failing here were both `0.0.2`. Comparing bytes is the only
    // answer that is always right.
    if matches_bundled(bundled, agents_dir) {
        // Byte-identical to what this build ships, so age is not the story and
        // saying otherwise would send the user to reinstall the same file.
        return None;
    }
    Some(format!(
        "this is the installed copy of the bundled '{}' agent, and it differs from the one this \
         build ships, so it is most likely out of date rather than broken. Run `lev setup` to \
         reinstall it, or `lev add <path>` if you meant to keep your own edits.",
        bundled.name
    ))
}

/// [`stale_install_hint`] as a suffix ready to append to an error message, or
/// an empty string when there is nothing to say.
///
/// Here rather than at each call site because both callers want the same
/// "hint or nothing" shape and differ only in how they separate it from the
/// error: `lev validate` prints a paragraph, the daemon writes one line.
pub fn stale_install_suffix(
    manifest_path: &Path,
    agents_dir: Option<&Path>,
    separator: &str,
) -> String {
    match stale_install_hint(manifest_path, agents_dir) {
        Some(hint) => format!("{separator}{hint}"),
        None => String::new(),
    }
}

/// The agents directory of the real environment, for a caller that has no test
/// seam of its own.
///
/// `None` when the home directory cannot be resolved, which
/// [`stale_install_hint`] reads as "nowhere to check" and stays quiet about.
pub fn real_agents_dir_opt() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| crate::commands::setup::real_agents_dir(Some(&h)))
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

    /// A bundled stage that routes tool output into a region must be able to
    /// read one.
    ///
    /// Routing leaves a pointer in the conversation saying where the output
    /// went. If the stage also grants a file-reading tool and no
    /// `context_read`, the only read verb the model has points at the
    /// filesystem, and it aims it at the region name: 90 of 168 failed
    /// `read_file` calls across 152 local runs were exactly that, one of them
    /// spending five turns on five spellings of `raw_findings`.
    ///
    /// Asserted over whatever is bundled rather than a fixed list of stages, so
    /// a new routed stage is held to it the day it lands.
    #[test]
    fn every_bundled_stage_that_routes_can_also_read_a_region() {
        let mut routed = 0;
        for agent in BUNDLED_AGENTS {
            let manifest = agent
                .files
                .iter()
                .find(|(rel, _)| *rel == "agent.leviath")
                .map(|(_, c)| *c)
                .expect("every bundled agent ships a manifest");
            let parsed = leviath_core::manifest::parse_manifest(manifest);
            assert!(
                parsed.is_ok(),
                "bundled agent {} does not parse",
                agent.name
            );
            let blueprint = parsed.expect("asserted Ok just above");

            for stage in &blueprint.stages {
                let routes = stage.tool_result_routing.as_ref().is_some_and(|r| {
                    r.default_region != "conversation"
                        || r.tool_overrides.values().any(|v| v != "conversation")
                });
                let reads_files = stage
                    .available_tools
                    .iter()
                    .any(|t| t == "read_file" || t == "read_files");
                if !routes || !reads_files {
                    continue;
                }
                routed += 1;
                assert!(
                    stage.available_tools.iter().any(|t| t == "context_read"),
                    "{}'s stage '{}' routes tool output into a region and grants a \
                     file-reading tool, but not 'context_read' - the only way the \
                     model can act on the pointer is to aim read_file at the region \
                     name",
                    agent.name,
                    stage.name
                );
            }
        }
        // A vacuous pass would be a bundled set that routes nowhere.
        assert!(
            routed > 0,
            "no bundled stage routes tool output to a region"
        );
    }

    /// A bundled layout must actually grow with the model's context window.
    ///
    /// Every region was written as `budget = "N%"` *and* an absolute
    /// `max_tokens`, and the cap is the smaller of the two on any window worth
    /// having: `researcher` ran on a 1,048,576-token model with `raw_findings`
    /// asking for 30% - 314,573 tokens - and getting the 40,000 its guard-rail
    /// allowed. The percentages were decorative from about 167k upward.
    ///
    /// Stated as a ratio rather than a per-region ceiling so it holds whatever
    /// the percentages are: resolve each layout against two windows a little
    /// over 5x apart, and the room must scale with them. A layout clamped by
    /// absolute caps scores 1.0 here, because both windows resolve to the same
    /// fixed numbers.
    ///
    /// Asserted over whatever is bundled, so a new agent is held to it the day
    /// it lands - and a deliberate ceiling on ONE region still passes, which is
    /// the point: this forbids a clamped layout, not a considered cap.
    #[test]
    fn every_bundled_layout_scales_with_the_model_window() {
        const NARROW: usize = 200_000;
        const WIDE: usize = 1_048_576;
        // The windows are 5.24x apart; require most of that to survive.
        const MIN_GROWTH: f64 = 4.0;

        let room = |layout: &leviath_core::ContextLayout, window: usize| -> usize {
            layout
                .resolved(window)
                .regions
                .iter()
                .map(|r| r.max_tokens)
                .sum()
        };

        let mut checked = 0;
        for agent in BUNDLED_AGENTS {
            let manifest = agent
                .files
                .iter()
                .find(|(rel, _)| *rel == "agent.leviath")
                .map(|(_, c)| *c)
                .expect("every bundled agent ships a manifest");
            let parsed = leviath_core::manifest::parse_manifest(manifest);
            assert!(
                parsed.is_ok(),
                "bundled agent {} does not parse",
                agent.name
            );
            let blueprint = parsed.expect("asserted Ok just above");

            // Percentage ceilings may sum past 100% on purpose (regions rarely
            // fill together), so the sum is not the thing to assert. What must
            // hold at every window is the layout's own validation: the fixed
            // pinned/hashmap/history regions have to leave the agent room to
            // work. Checked across the range a real model spans, because the
            // floors bind at the bottom of it and the percentages at the top.
            for window in [32_768, 128_000, NARROW, WIDE] {
                // The shared layout plus any per-stage override, as one
                // sequence: a bundled agent may declare either, and a branch
                // for the per-stage case would sit unreached while none does.
                let layouts = std::iter::once(&blueprint.context_layout).chain(
                    blueprint
                        .stages
                        .iter()
                        .filter_map(|s| s.context_layout.as_ref()),
                );
                for layout in layouts {
                    assert!(
                        layout.resolved(window).validate().is_ok(),
                        "a layout of {} does not validate at a {window}-token window",
                        agent.name
                    );
                }
            }

            let narrow = room(&blueprint.context_layout, NARROW);
            let wide = room(&blueprint.context_layout, WIDE);
            let growth = wide as f64 / narrow as f64;
            assert!(
                growth >= MIN_GROWTH,
                "{}'s context layout barely grows between a narrow window and a \
                 wide one (growth {growth:.2}x, wanted at least {MIN_GROWTH}x, \
                 {narrow} -> {wide} tokens of region room). An absolute \
                 max_tokens is overriding the percentage budgets.",
                agent.name
            );
            checked += 1;
        }
        // A vacuous pass would be a loop over nothing.
        assert!(checked > 0, "no bundled agent was checked");
    }

    /// Every bundled agent ends in a stage that hands something back, and
    /// nothing upstream can end the run before reaching it.
    ///
    /// The second half is the part that fails quietly. `allow_complete` on any
    /// earlier stage offers the model a "DONE" it can pick instead of routing
    /// onward - and it is appended even to a stage's custom `transition_prompt`,
    /// so a blueprint can offer an exit its own prompt never mentions. A run
    /// that takes it finishes with no answer, looking exactly like success.
    /// That happened to a shipped blueprint while this was being written.
    ///
    /// Asserted over whatever is bundled rather than a hard-coded list, so a
    /// new agent is held to it the day it lands.
    #[test]
    fn every_bundled_agent_ends_by_handing_something_back() {
        for agent in BUNDLED_AGENTS {
            let manifest = agent
                .files
                .iter()
                .find(|(rel, _)| *rel == "agent.leviath")
                .map(|(_, c)| *c)
                .expect("checked above");
            let blueprint = leviath_core::manifest::parse_manifest(manifest)
                .expect("checked by every_bundled_manifest_parses");

            let outputs: Vec<&leviath_core::Stage> = blueprint
                .stages
                .iter()
                .filter(|s| s.mode == leviath_core::blueprint::StageMode::Output)
                .collect();
            assert!(
                !outputs.is_empty(),
                "bundled agent {} has no output stage, so a run of it hands back nothing",
                agent.name
            );

            for stage in &outputs {
                // The mode is meant to imply all three; a stage where it did
                // not would advertise a tool it is not required to call.
                assert!(stage.require_output, "{} output stage", agent.name);
                assert!(
                    stage
                        .available_tools
                        .iter()
                        .any(|t| t == leviath_core::blueprint::SUBMIT_OUTPUT_TOOL),
                    "{} output stage cannot submit",
                    agent.name
                );
                // A stage whose job is to report has no business writing files.
                assert!(
                    !stage.available_tools.iter().any(|t| {
                        leviath_core::blueprint::MODIFYING_TOOLS
                            .contains(&leviath_tools::canonical_tool_name(t))
                    }),
                    "{} output stage can modify files",
                    agent.name
                );
            }

            for stage in &blueprint.stages {
                assert!(
                    !stage.allow_complete
                        || stage.mode == leviath_core::blueprint::StageMode::Output,
                    "bundled agent {}: stage '{}' may end the run, skipping the output stage",
                    agent.name,
                    stage.name
                );
            }
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    fn key() -> String {
        "not-used-offline".to_string()
    }

    /// Every provider `lev setup` can configure, built so a test can ask them
    /// what they serve. Claude Code is a transport rather than a provider a
    /// stage names, so it is not in this list.
    ///
    /// The keys are never used: `serves_model` reads a table compiled into the
    /// build, which is exactly the offline answer wanted here.
    fn setup_providers() -> Vec<(&'static str, Box<dyn leviath_providers::Provider>)> {
        let client = reqwest::Client::new();
        let key = || "not-used-offline".to_string();
        vec![
            (
                "anthropic",
                Box::new(leviath_providers::AnthropicProvider::new(
                    client.clone(),
                    key(),
                )) as Box<dyn leviath_providers::Provider>,
            ),
            (
                "openai",
                Box::new(leviath_providers::OpenAIProvider::new(
                    client.clone(),
                    key(),
                )),
            ),
            (
                "google",
                Box::new(leviath_providers::GeminiProvider::new(
                    client.clone(),
                    key(),
                )),
            ),
            (
                "openrouter",
                Box::new(leviath_providers::OpenRouterProvider::new(
                    client.clone(),
                    key(),
                )),
            ),
            (
                "ollama",
                Box::new(leviath_providers::OllamaProvider::new(client)),
            ),
        ]
    }

    /// The published JSON Schema for `agent.leviath`.
    ///
    /// Compiled into the test so it cannot drift from the file that ships:
    /// this is the same text served at
    /// `https://leviath.dev/docs/<channel>/blueprint.schema.json`.
    const BLUEPRINT_SCHEMA: &str = include_str!("../../../docs/schema/blueprint.schema.json");

    /// Every way `value` fails `validator`, as readable lines.
    ///
    /// Shared by the positive and negative tests so the formatting closure runs
    /// against real errors. Called only from the passing path of each, because
    /// a call inside an `assert!` message is a region only failure reaches.
    fn schema_problems(
        validator: &jsonschema::Validator,
        value: &serde_json::Value,
    ) -> Vec<String> {
        validator
            .iter_errors(value)
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect()
    }

    /// Convert parsed TOML to JSON so a JSON Schema can be applied to it.
    fn toml_to_json(value: &toml::Value) -> serde_json::Value {
        match value {
            toml::Value::String(s) => serde_json::Value::String(s.clone()),
            toml::Value::Integer(i) => serde_json::Value::from(*i),
            toml::Value::Float(f) => serde_json::Value::from(*f),
            toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
            // A TOML datetime has no JSON counterpart; the blueprint format has
            // no datetime-valued key, so rendering it as its own text is enough
            // for the schema to reject it wherever it appears.
            toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
            toml::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(toml_to_json).collect())
            }
            toml::Value::Table(table) => serde_json::Value::Object(
                table
                    .iter()
                    .map(|(k, v)| (k.clone(), toml_to_json(v)))
                    .collect(),
            ),
        }
    }

    #[test]
    fn toml_converts_to_json_for_every_value_kind() {
        // Every arm, because a kind converted wrongly would be validated
        // against the wrong JSON type and the schema would pass or fail for the
        // wrong reason. `temperature` is a real float-valued blueprint key, so
        // that arm is not hypothetical.
        let source = concat!(
            "s = \"text\"\n",
            "i = 7\n",
            "f = 0.5\n",
            "b = true\n",
            "d = 1979-05-27T07:32:00Z\n",
            "a = [1, \"two\"]\n",
            "[t]\n",
            "nested = 1\n"
        );
        let parsed: toml::Value = toml::from_str(source).expect("valid TOML");
        let json = toml_to_json(&parsed);
        assert_eq!(json["s"], serde_json::json!("text"));
        assert_eq!(json["i"], serde_json::json!(7));
        assert_eq!(json["f"], serde_json::json!(0.5));
        assert_eq!(json["b"], serde_json::json!(true));
        // No JSON counterpart for a datetime, so it becomes its own text.
        assert!(json["d"].is_string());
        assert_eq!(json["a"], serde_json::json!([1, "two"]));
        assert_eq!(json["t"]["nested"], serde_json::json!(1));
    }

    #[test]
    fn every_bundled_blueprint_validates_against_the_published_schema() {
        // The schema is the only machine-readable description of this format,
        // and an agent authoring a blueprint will write against it. Nothing but
        // this test keeps it honest: the parser is a hand-rolled toml::Value
        // walker, so there is no derive to generate it from.
        let schema: serde_json::Value =
            serde_json::from_str(BLUEPRINT_SCHEMA).expect("the schema is valid JSON");
        let validator = jsonschema::validator_for(&schema).expect("the schema compiles");

        for agent in BUNDLED_AGENTS {
            let manifest = agent
                .files
                .iter()
                .find(|(rel, _)| *rel == "agent.leviath")
                .map(|(_, c)| *c)
                .expect("every bundled agent has a manifest");
            let parsed: toml::Value = toml::from_str(manifest).expect("the manifest is valid TOML");
            let json = toml_to_json(&parsed);

            assert_eq!(
                schema_problems(&validator, &json),
                Vec::<String>::new(),
                "{} does not match blueprint.schema.json",
                agent.name
            );
        }
    }

    #[test]
    fn the_blueprint_schema_accepts_every_region_kind_the_parser_names() {
        // The bundled agents between them use only some of the kinds, so the
        // positive test above cannot notice one the schema forgot. `checklist`
        // shipped that way: the parser took it, the published schema's closed
        // enum refused it, and every blueprint using the feature failed to
        // validate against the file we tell people to validate against.
        //
        // The parser's own error message enumerates the valid kinds, so read
        // the list back from it rather than restating it here and drifting the
        // same way twice.
        let err = leviath_core::manifest::parse_manifest(
            "[agent]\nname = \"a\"\n\n[context.regions]\nx = { kind = \"not-a-kind\" }\n",
        )
        .expect_err("an unknown region kind is a load error")
        .to_string();
        let listed = err
            .split("valid kinds:")
            .nth(1)
            .expect("the error names the valid kinds")
            .trim()
            .trim_end_matches(')')
            .split(',')
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .collect::<Vec<_>>();
        assert!(
            listed.len() > 5,
            "the error should list every kind: {listed:?}"
        );

        let schema: serde_json::Value =
            serde_json::from_str(BLUEPRINT_SCHEMA).expect("the schema is valid JSON");
        let validator = jsonschema::validator_for(&schema).expect("the schema compiles");
        for kind in listed {
            let manifest = format!(
                "[agent]\nname = \"a\"\n\n[context.regions]\nx = {{ kind = \"{kind}\" }}\n"
            );
            let parsed: toml::Value = toml::from_str(&manifest).expect("valid TOML");
            assert_eq!(
                schema_problems(&validator, &toml_to_json(&parsed)),
                Vec::<String>::new(),
                "the schema rejects region kind \"{kind}\", which the parser accepts"
            );
        }
    }

    #[test]
    fn the_blueprint_schema_accepts_every_transition_condition_the_parser_names() {
        // The third instance of the same drift: `dead_end` parsed, the
        // `dead-end-possible` lint told people to write it, and the published
        // schema's closed enum rejected it. Same trick as the region kinds, for
        // the same reason: read the list out of the parser's error rather than
        // restating it here.
        let err = leviath_core::manifest::parse_manifest(
            "[agent]\nname = \"a\"\n\n[stages.main.transitions.other]\ncondition = \"whenever\"\n",
        )
        .expect_err("an unknown condition is a load error")
        .to_string();
        let listed = err
            .split("(valid:")
            .nth(1)
            .expect("the error names the valid conditions")
            .trim()
            .trim_end_matches(')')
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect::<Vec<_>>();
        assert!(
            listed.len() > 3,
            "the error should list every condition: {listed:?}"
        );

        let schema: serde_json::Value =
            serde_json::from_str(BLUEPRINT_SCHEMA).expect("the schema is valid JSON");
        let validator = jsonschema::validator_for(&schema).expect("the schema compiles");
        for condition in listed {
            let manifest = format!(
                "[agent]\nname = \"a\"\n\n[stages.main.transitions.other]\ncondition = \"{condition}\"\n"
            );
            let parsed: toml::Value = toml::from_str(&manifest).expect("valid TOML");
            assert_eq!(
                schema_problems(&validator, &toml_to_json(&parsed)),
                Vec::<String>::new(),
                "the schema rejects condition \"{condition}\", which the parser accepts"
            );
        }
    }

    #[test]
    fn the_blueprint_schema_accepts_stage_hooks() {
        // Same blind spot as the region kinds: no bundled agent declares hooks,
        // so nothing noticed that the stage object is `additionalProperties:
        // false` without a `hooks` property. Every blueprint following the Rhai
        // hooks page was rejected by the published schema.
        let schema: serde_json::Value =
            serde_json::from_str(BLUEPRINT_SCHEMA).expect("the schema is valid JSON");
        let validator = jsonschema::validator_for(&schema).expect("the schema compiles");
        let manifest = "[agent]\nname = \"a\"\n\n[stages.main.hooks]\n\
                        on_stage_enter = \"hooks/enter.rhai\"\n\
                        on_error = \"hooks/error.rhai\"\n";
        let parsed: toml::Value = toml::from_str(manifest).expect("valid TOML");
        assert_eq!(
            schema_problems(&validator, &toml_to_json(&parsed)),
            Vec::<String>::new(),
            "the schema rejects [stages.<name>.hooks], which the parser accepts"
        );
    }

    #[test]
    fn the_blueprint_schema_rejects_what_the_parser_rejects() {
        // A schema that accepts everything would pass the test above over any
        // input at all. These are the mistakes it exists to catch before a run
        // is ever spawned.
        let schema: serde_json::Value =
            serde_json::from_str(BLUEPRINT_SCHEMA).expect("the schema is valid JSON");
        let validator = jsonschema::validator_for(&schema).expect("the schema compiles");
        // Goes through `schema_problems` rather than `is_valid`, so the same
        // error-formatting path the positive test uses is actually exercised
        // by something that produces errors.
        let rejects = |manifest: &str| {
            let parsed: toml::Value = toml::from_str(manifest).expect("valid TOML");
            !schema_problems(&validator, &toml_to_json(&parsed)).is_empty()
        };

        assert!(
            rejects("[stages.main]\nmode = \"autonomous\"\n"),
            "no [agent]"
        );
        assert!(
            rejects("[agent]\nname = \"a\"\n\n[context.regions]\nx = { kind = \"nonsense\" }\n"),
            "unknown region kind"
        );
        assert!(
            rejects(
                "[agent]\nname = \"a\"\n\n[stages.main.transitions.other]\ncondition = \"whenever\"\n"
            ),
            "unknown transition condition"
        );
        assert!(
            rejects("[agent]\nname = \"a\"\n\n[stages.main]\nmax_iteratoins = 5\n"),
            "a typo'd stage key"
        );
        assert!(
            rejects("[agent]\nname = \"a\"\n\n[tool_permissions]\nshell = \"maybe\"\n"),
            "an invalid tool policy"
        );
        // And the minimum the parser accepts still passes, so the rules above
        // are not rejecting everything.
        assert!(!rejects("[agent]\nname = \"a\"\n"), "a minimal manifest");
    }

    /// A declared `gate` must actually check something.
    ///
    /// Its fields are parsed one by one, so a misspelt condition leaves them all
    /// at their defaults and the gate passes every time. The edge still parses
    /// and still validates, which is exactly how a gate silently stops gating.
    #[test]
    fn a_declared_gate_never_parses_to_one_that_checks_nothing() {
        for agent in BUNDLED_AGENTS {
            let manifest = agent
                .files
                .iter()
                .find(|(rel, _)| *rel == "agent.leviath")
                .map(|(_, c)| *c)
                .expect("every bundled agent has a manifest");
            // The raw text says which edges *declare* a gate; the parsed
            // blueprint says what those gates actually check. Comparing the two
            // is the point: a gate that parses to nothing is invisible in the
            // blueprint alone.
            let declared = manifest.matches("gate = {").count();
            let blueprint =
                leviath_core::manifest::parse_manifest(manifest).expect("manifest parses");
            let mut checking = 0;
            for stage in &blueprint.stages {
                for (target, edge) in stage.transitions.iter().flatten() {
                    let Some(gate) = &edge.gate else { continue };
                    let checks = gate.require_modifications
                        || gate.region.is_some()
                        || gate.require_region_updated.is_some();
                    assert!(
                        checks,
                        "{}: {} -> {} declares a gate that checks nothing, so it \
                         passes every time; check the spelling of its keys",
                        agent.name, stage.name, target
                    );
                    checking += 1;
                }
            }
            assert_eq!(
                declared, checking,
                "{}: {declared} edges write `gate = {{`, but {checking} parsed \
                 into a gate - the difference is gates that were dropped",
                agent.name
            );
        }
    }

    /// Every stage resolves to the first model it lists that anything serves.
    ///
    /// A blueprint's `models` is a preference order. The host picks a route
    /// within that order; it does not get to pick a different preference. #578
    /// was exactly that failure: entries matched as whole `provider/model`
    /// pairs, so `default_provider` chose among routes and whatever model its
    /// route happened to name came back. `polish` asked for
    /// `gemini-3.1-pro-preview` and ran `claude-sonnet-5`.
    ///
    /// Run over every bundled agent, because the blueprints are what ship.
    #[test]
    fn every_bundled_stage_runs_the_first_model_anything_serves() {
        let providers = setup_providers();
        let registry = {
            let mut r = leviath_runtime::ProviderRegistry::new();
            for (name, _) in &providers {
                // The same set, as the resolver sees them.
                let p: std::sync::Arc<dyn leviath_providers::Provider> = match *name {
                    "anthropic" => std::sync::Arc::new(leviath_providers::AnthropicProvider::new(
                        client(),
                        key(),
                    )),
                    "openai" => {
                        std::sync::Arc::new(leviath_providers::OpenAIProvider::new(client(), key()))
                    }
                    "google" => {
                        std::sync::Arc::new(leviath_providers::GeminiProvider::new(client(), key()))
                    }
                    "openrouter" => std::sync::Arc::new(
                        leviath_providers::OpenRouterProvider::new(client(), key()),
                    ),
                    _ => std::sync::Arc::new(leviath_providers::OllamaProvider::new(client())),
                };
                r.register((*name).to_string(), p);
            }
            r
        };

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
                // Kept rather than found: filtering evaluates the predicate for
                // every entry, so the arm handling a pinned route is exercised by
                // the entries that pin one instead of being skipped the moment an
                // earlier open entry matches.
                let reachable: Vec<&leviath_core::blueprint::ModelEntry> = stage
                    .model
                    .models
                    .iter()
                    .filter(|e| {
                        let k = e.model.rsplit('/').next().unwrap_or(&e.model);
                        match e.provider.is_empty() {
                            true => providers.iter().any(|(_, p)| p.serves_model(k).is_some()),
                            false => providers.iter().any(|(n, _)| *n == e.provider),
                        }
                    })
                    .collect();
                let listed: Vec<&str> = stage
                    .model
                    .models
                    .iter()
                    .map(|e| e.model.as_str())
                    .collect();
                assert!(
                    !reachable.is_empty(),
                    "{}/{} lists {listed:?} and this registry reaches none of them",
                    agent.name,
                    stage.name,
                );
                let want_key = reachable[0]
                    .model
                    .rsplit('/')
                    .next()
                    .unwrap_or(&reachable[0].model);

                // A real default provider, because the reordering #578 lived in
                // only runs when one is set and registered. Passing the empty
                // default skips that block entirely and tests nothing.
                let defaults = leviath_runtime::pipeline::ModelDefaults {
                    provider: "openrouter".to_string(),
                    ..Default::default()
                };
                let (got_provider, got_model) = leviath_runtime::pipeline::resolve_stage_model(
                    &stage.model,
                    None,
                    &defaults,
                    &registry,
                );
                let got_key = got_model.rsplit('/').next().unwrap_or(&got_model);

                assert_eq!(
                    got_key, want_key,
                    "{}/{} lists {listed:?} and the first one reachable is \
                     {want_key}, but it resolved to {got_key} on {got_provider}: \
                     the host chose a different model, not a different route",
                    agent.name, stage.name,
                );
            }
        }
    }

    /// A provider claims the models it serves, and not the rest.
    ///
    /// `serves_model` decides which provider a bare model name resolves to, so a
    /// provider that over-claims wins models it cannot run. That is the same
    /// failure #578 was about, arriving from the other direction: there the host
    /// picked a route and got the wrong model, here a provider claims a model it
    /// has never heard of.
    #[test]
    fn a_provider_does_not_claim_models_from_other_vendors() {
        let providers = setup_providers();
        let get = |want: &str| {
            providers
                .iter()
                .find(|(n, _)| *n == want)
                .map(|(_, p)| p)
                .expect("configured above")
        };

        // Each of these is unmistakably one vendor's.
        for (owner, model) in [
            ("anthropic", "claude-opus-5"),
            ("openai", "gpt-5.5"),
            ("google", "gemini-3.1-pro-preview"),
        ] {
            assert!(
                get(owner).serves_model(model).is_some(),
                "{owner} should serve its own {model}"
            );
            for other in ["anthropic", "openai", "google"] {
                if other == owner {
                    continue;
                }
                assert!(
                    get(other).serves_model(model).is_none(),
                    "{other} claims {model}, which belongs to {owner}: a bare \
                     model name would resolve to a provider that cannot run it"
                );
            }
        }

        // And nobody claims a model that does not exist.
        for name in ["anthropic", "openai", "google"] {
            assert!(
                get(name).serves_model("not-a-real-model-xyz").is_none(),
                "{name} claims a model nobody has"
            );
        }
    }

    /// A `transform = "custom"` edge must actually name regions.
    ///
    /// `transform_config` is parsed key by key, so a misspelt key leaves every
    /// list empty and the edge becomes an expensive no-op that still parses and
    /// still validates. Discovered from BUNDLED_AGENTS so it covers whatever
    /// ships, not a list kept by hand.
    #[test]
    fn a_custom_transform_never_parses_to_nothing() {
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
                for (target, edge) in stage.transitions.iter().flatten() {
                    let leviath_core::blueprint::EdgeTransform::Custom {
                        carry,
                        compact,
                        clear,
                        ..
                    } = &edge.transform
                    else {
                        continue;
                    };
                    assert!(
                        !(carry.is_empty() && compact.is_empty() && clear.is_empty()),
                        "{}: {} -> {} declares a custom transform that names no \
                         regions, so it does nothing; check the spelling of the \
                         keys under transform_config",
                        agent.name,
                        stage.name,
                        target
                    );
                }
            }
        }
    }

    #[test]
    fn every_bundled_stage_offers_every_provider_setup_can_configure() {
        // Getting Started promises that one provider is all you need: on a
        // machine configured with exactly one of them, every stage still runs.
        //
        // Asked of the providers themselves rather than of the spelling. A
        // blueprint names models and leaves routing to the machine, so "does
        // this stage name a provider" no longer answers the question - "does
        // this provider serve anything this stage named" does.
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
                let providers = setup_providers();
                // A gateway fronts the other vendors, so it reaches whatever
                // they reach. Its own answer comes from a catalogue fetched at
                // startup, which a test with no network cannot consult.
                let native = |key: &str| {
                    providers
                        .iter()
                        .any(|(n, p)| *n != "openrouter" && p.serves_model(key).is_some())
                };
                for (name, provider) in &providers {
                    let reachable = stage.model.models.iter().any(|entry| {
                        if !entry.provider.is_empty() {
                            return entry.provider == *name;
                        }
                        let key = entry.model.rsplit('/').next().unwrap_or(&entry.model);
                        if *name == "openrouter" {
                            return native(key);
                        }
                        provider.serves_model(key).is_some()
                    });
                    assert!(
                        reachable,
                        "{}/{} names nothing {} can run, so a machine holding \
                         only that provider cannot reach this stage: {:?}",
                        agent.name, stage_name, name, stage.model.models
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
            safe_commands_granted: None,
            model_windows: crate::commands::models::builtin_model_windows(),
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

    /// The limitation this closes: comparing versions alone meant a blueprint
    /// edited without a version bump read as current forever, so the user was
    /// never told their copy had drifted from the one that shipped.
    #[test]
    fn plan_reports_an_edited_install_as_modified() {
        let dir = tempfile::tempdir().unwrap();
        let agent = &BUNDLED_AGENTS[0];
        install_bundled(agent, dir.path()).unwrap();
        let manifest_path = dir.path().join(agent.name).join("agent.leviath");
        let manifest = std::fs::read_to_string(&manifest_path).unwrap();
        std::fs::write(&manifest_path, manifest + "\n# a local edit\n").unwrap();

        let action = action_for(&plan_agent_actions(dir.path()), agent.name);
        assert_eq!(action, AgentAction::Modified);
        // It would change disk, so the wizard offers it - but never unasked,
        // because reinstalling destroys the edit.
        assert!(action.is_change());
        assert!(!action.preselect());
        let label = action.label(agent.version);
        assert!(label.contains("edited locally"), "{label}");
    }

    /// `install_bundled` removes the destination first, so a file the user
    /// added is destroyed by a reinstall too - which makes it exactly as
    /// important to notice as an edited one.
    #[test]
    fn a_file_the_user_added_or_removed_counts_as_modified() {
        let agent = &BUNDLED_AGENTS[0];

        let added = tempfile::tempdir().unwrap();
        install_bundled(agent, added.path()).unwrap();
        std::fs::write(added.path().join(agent.name).join("notes.md"), "mine").unwrap();
        assert_eq!(
            action_for(&plan_agent_actions(added.path()), agent.name),
            AgentAction::Modified
        );

        // A file deleted from a blueprint that ships more than the manifest.
        // The manifest still parses at the bundled version, so only the file
        // comparison can catch this.
        let multi = BUNDLED_AGENTS
            .iter()
            .find(|a| a.files.len() > 1)
            .expect("some bundled blueprint ships more than its manifest");
        let removed = tempfile::tempdir().unwrap();
        install_bundled(multi, removed.path()).unwrap();
        let extra = multi
            .files
            .iter()
            .map(|(rel, _)| *rel)
            .find(|rel| *rel != "agent.leviath")
            .expect("a file other than the manifest");
        std::fs::remove_file(removed.path().join(multi.name).join(extra)).unwrap();
        assert_eq!(
            action_for(&plan_agent_actions(removed.path()), multi.name),
            AgentAction::Modified
        );
    }

    /// A directory that cannot be walked reads as differing, which is the safe
    /// direction: this decides whether overwriting is safe.
    #[test]
    fn an_unreadable_tree_is_not_up_to_date() {
        assert_eq!(installed_file_count(Path::new("/no/such/dir")), 0);
        let dir = tempfile::tempdir().unwrap();
        assert!(!matches_bundled(&BUNDLED_AGENTS[0], dir.path()));
    }

    #[test]
    fn installed_file_count_walks_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("top.txt"), "x").unwrap();
        std::fs::write(dir.path().join("a/mid.txt"), "x").unwrap();
        std::fs::write(dir.path().join("a/b/leaf.txt"), "x").unwrap();
        assert_eq!(installed_file_count(dir.path()), 3);
    }

    fn action_for(plan: &[(&'static BundledAgent, AgentAction)], name: &str) -> AgentAction {
        plan.iter()
            .find(|(a, _)| a.name == name)
            .expect("the bundled agent is in the plan")
            .1
            .clone()
    }

    // ─── stale_install_hint ─────────────────────────────────────────────────

    /// The report that prompted this: a user on alpha whose installed `coder`
    /// stopped loading, with an error about graph shape and nothing saying the
    /// file was simply old. A blueprint that predates a graph rule fails in a
    /// way that reads as a broken agent rather than an out-of-date one.
    #[test]
    fn an_installed_agent_that_will_not_load_is_named_as_out_of_date() {
        let dir = tempfile::tempdir().unwrap();
        let agent = &BUNDLED_AGENTS[0];
        install_bundled(agent, dir.path()).unwrap();
        let manifest = dir.path().join(agent.name).join("agent.leviath");

        // Byte-identical to what this build ships: age is not the story, and
        // sending the user to reinstall the same file would waste their time.
        assert_eq!(stale_install_hint(&manifest, Some(dir.path())), None);

        // Any difference is enough. The version field is deliberately not
        // consulted, because it routinely does not move when the file does:
        // both coder blueprints in the report were `0.0.2`.
        std::fs::write(&manifest, "[agent]\nname = \"x\"\nversion = \"0.0.2\"\n").unwrap();
        let hint =
            stale_install_hint(&manifest, Some(dir.path())).expect("a changed copy is named");
        assert!(hint.contains(agent.name), "{hint}");
        assert!(hint.contains("lev setup"), "{hint}");
    }

    /// Narrow in the same way as the note: it speaks only for the installed
    /// copy, so a blueprint of the user's own is never blamed on a bundled one
    /// that shares its name, and neither is a path with no agents dir to check.
    /// The suffix form is what both call sites actually use, and the thing that
    /// must not decorate an error with a blank paragraph when there is no hint.
    #[test]
    fn the_suffix_carries_the_hint_or_nothing_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let agent = &BUNDLED_AGENTS[0];
        install_bundled(agent, dir.path()).unwrap();
        let manifest = dir.path().join(agent.name).join("agent.leviath");

        // Nothing to say: an empty string, not a separator with nothing after it.
        assert_eq!(
            stale_install_suffix(&manifest, Some(dir.path()), "\n\n"),
            ""
        );

        std::fs::write(&manifest, "[agent]\nname = \"x\"\n").unwrap();
        let suffix = stale_install_suffix(&manifest, Some(dir.path()), "\n\n");
        assert!(suffix.starts_with("\n\n"), "{suffix:?}");
        assert!(suffix.contains(agent.name), "{suffix:?}");
        // The daemon writes one line rather than a paragraph, same hint.
        assert!(
            stale_install_suffix(&manifest, Some(dir.path()), ". ").starts_with(". "),
            "the separator is the caller's choice"
        );
    }

    #[test]
    fn the_hint_stays_quiet_outside_the_installed_copy() {
        let dir = tempfile::tempdir().unwrap();
        let agent = &BUNDLED_AGENTS[0];
        install_bundled(agent, dir.path()).unwrap();

        let elsewhere = dir.path().join("elsewhere").join(agent.name);
        std::fs::create_dir_all(&elsewhere).unwrap();
        let mine = elsewhere.join("agent.leviath");
        std::fs::write(&mine, "[agent]\nname = \"mine\"\n").unwrap();
        assert_eq!(stale_install_hint(&mine, Some(dir.path())), None);

        // A name no bundled agent has, inside the agents dir.
        let other = dir.path().join("not-a-bundled-agent");
        std::fs::create_dir_all(&other).unwrap();
        let manifest = other.join("agent.leviath");
        std::fs::write(&manifest, "[agent]\nname = \"other\"\n").unwrap();
        assert_eq!(stale_install_hint(&manifest, Some(dir.path())), None);

        // And with nowhere to look, it says nothing rather than guessing.
        assert_eq!(
            stale_install_hint(&dir.path().join(agent.name).join("agent.leviath"), None),
            None
        );
    }

    // ─── stale_install_note ─────────────────────────────────────────────────

    /// The case that prompted this: an install sitting versions behind, with
    /// nothing saying so at the moment it mattered.
    #[test]
    fn a_stale_install_is_named_when_the_run_starts() {
        let dir = tempfile::tempdir().unwrap();
        let agent = &BUNDLED_AGENTS[0];
        install_bundled(agent, dir.path()).unwrap();
        let manifest = dir.path().join(agent.name).join("agent.leviath");
        let mut blueprint =
            leviath_core::manifest::parse_manifest(&std::fs::read_to_string(&manifest).unwrap())
                .unwrap();

        // At the bundled version there is nothing to say.
        assert_eq!(
            stale_install_note(&manifest, &blueprint, Some(dir.path())),
            None
        );

        blueprint.version = "0.0.1".to_string();
        let note = stale_install_note(&manifest, &blueprint, Some(dir.path()))
            .expect("a behind install is named");
        assert!(note.contains("0.0.1"), "{note}");
        assert!(note.contains(agent.version), "{note}");
        assert!(note.contains("lev setup"), "{note}");
    }

    /// Deliberately narrow: a blueprint of the user's own that happens to share
    /// a name with a bundled one is never nagged about, and neither is one this
    /// build does not ship.
    #[test]
    fn a_blueprint_that_is_not_the_installed_copy_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let agent = &BUNDLED_AGENTS[0];
        install_bundled(agent, dir.path()).unwrap();
        let manifest = dir.path().join(agent.name).join("agent.leviath");
        let mut blueprint =
            leviath_core::manifest::parse_manifest(&std::fs::read_to_string(&manifest).unwrap())
                .unwrap();
        blueprint.version = "0.0.1".to_string();

        // Somewhere else on disk, under the same name.
        let elsewhere = tempfile::tempdir().unwrap();
        let copy = elsewhere.path().join(agent.name).join("agent.leviath");
        assert_eq!(
            stale_install_note(&copy, &blueprint, Some(dir.path())),
            None,
            "not the installed copy"
        );

        // No agents dir resolves at all.
        assert_eq!(stale_install_note(&manifest, &blueprint, None), None);

        // A name this build ships nothing for.
        blueprint.name = "not-a-bundled-agent".to_string();
        assert_eq!(
            stale_install_note(
                &dir.path().join("not-a-bundled-agent").join("agent.leviath"),
                &blueprint,
                Some(dir.path())
            ),
            None
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
