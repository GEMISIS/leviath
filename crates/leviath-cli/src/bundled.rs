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
    pub(crate) fn is_change(&self) -> bool {
        !matches!(self, Self::UpToDate)
    }

    /// Whether the wizard should pre-check this row.
    ///
    /// Not the same question as [`Self::is_change`], and the difference is the
    /// point of [`Self::Modified`]: reinstalling over a tree the user edited
    /// destroys their work, and `install_bundled` removes the destination
    /// first, so it destroys files they added too. Offered, never assumed.
    pub(crate) fn preselect(&self) -> bool {
        matches!(self, Self::Install | Self::Update { .. })
    }

    /// Short label for the wizard's blueprint list.
    pub(crate) fn label(&self, to: &str) -> String {
        match self {
            Self::Install => format!("install {to}"),
            Self::Update { from } => format!("update {from} → {to}"),
            Self::Modified => format!("{to}, edited locally - reinstall overwrites"),
            Self::UpToDate => "up to date".to_string(),
        }
    }
}

/// The installed version of `name` under `agents_dir`, if a readable
/// `agent.toml` is there.
///
/// Deliberately lenient: a blueprint directory whose file is missing or whose
/// `[blueprint]` table does not read counts as *not installed*, so the wizard
/// offers a clean reinstall instead of refusing to plan. An unreadable file is
/// exactly the state a half-finished copy leaves behind. Only the
/// `[blueprint]` table is read: an install whose graph a newer build refuses
/// still has a version to offer an update over.
pub(crate) fn installed_version(agents_dir: &Path, name: &str) -> Option<String> {
    let text =
        std::fs::read_to_string(agents_dir.join(name).join(leviath_blueprint::FILE_NAME)).ok()?;
    leviath_blueprint::BlueprintMeta::read(&text)
        .ok()
        .map(|meta| meta.version)
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
pub(crate) fn plan_agent_actions(agents_dir: &Path) -> Vec<(&'static BundledAgent, AgentAction)> {
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
/// Deliberately narrow. It fires only for a blueprint at `path` that *is* the
/// installed copy, under `agents_dir/<name>/`, so a blueprint of the user's
/// own that happens to share a name with a bundled one is never nagged about.
/// `name` and `version` are what the blueprint says of itself.
pub(crate) fn stale_install_note(
    path: &Path,
    name: &str,
    version: &str,
    agents_dir: Option<&Path>,
) -> Option<String> {
    let installed = agents_dir?.join(name);
    if !path.starts_with(&installed) {
        return None;
    }
    let bundled = BUNDLED_AGENTS.iter().find(|a| a.name == name)?;
    if bundled.version == version {
        return None;
    }
    Some(format!(
        "note: '{name}' is installed at {version}, and this build ships {}. \
         Run `lev setup` to update it.",
        bundled.version
    ))
}

/// Why an installed bundled blueprint would not load, when the reason is that
/// it is old rather than that it is wrong.
///
/// The twin of [`stale_install_note`], for the path where there is no
/// blueprint to hand because reading or checking it is what failed. That is exactly when the user most needs to hear it: a graph rule
/// added after their install turns their copy into "invalid blueprint", which
/// reads as a bug in the agent rather than as an out-of-date file, and the
/// version note they would have got on the success path never fires.
///
/// Narrow in the same way: the file must *be* the installed copy at
/// `agents_dir/<name>/`, so a blueprint of the user's own is never blamed on a
/// bundled one that shares its name.
pub(crate) fn stale_install_hint(
    manifest_path: &Path,
    agents_dir: Option<&Path>,
) -> Option<String> {
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
pub(crate) fn stale_install_suffix(
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
pub(crate) fn real_agents_dir_opt() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| crate::commands::setup::real_agents_dir(Some(&h)))
}

/// Write one bundled blueprint into `<agents_dir>/<name>/`, replacing whatever
/// is there.
///
/// The existing tree is removed first rather than merged over: a stale file
/// from an older version of the blueprint (a tool script that was dropped, say)
/// would otherwise survive forever and keep being loaded. This mirrors what
/// `lev add`'s directory install already does.
pub(crate) fn install_bundled(agent: &BundledAgent, agents_dir: &Path) -> anyhow::Result<()> {
    let dest = agents_dir.join(agent.name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    for (rel, contents) in agent.files {
        // Derive the parent from the *relative* path rather than calling
        // `path.parent()`. `dest.join(rel)` always has a parent, so the `None`
        // arm of `parent()` would be unreachable code pretending to be a
        // handled case; splitting `rel` gives two arms that both actually
        // happen - nested (`tools/web_fetch.rhai`) and flat (`agent.toml`).
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
#[path = "bundled_tests.rs"]
mod tests;
