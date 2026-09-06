//! Config migrations `lev update` makes on the user's behalf, and the config
//! reading that decides which apply.
//!
//! Split out of `update.rs` by concern. The migration table is here; the
//! command's plan/report/execute flow that consumes it stays in the parent.

use std::path::Path;

use super::{UpdateArgs, UpdateEnv, UpdatePlan, agreed};
use crate::config::Config;
use crate::config::renamed::legacy_keys_present;

// ─── Config migrations ────────────────────────────────────────────────────────

/// One config change `lev update` knows how to make on the user's behalf.
///
/// The mechanism exists so that a future incompatibility - a key that moved, a
/// value whose meaning changed - is either fixed automatically or at least
/// explained at the moment the user updates into it, rather than surfacing as a
/// broken run days later. A key that only changed name is one entry in
/// [`crate::config::renamed::RENAMED_KEYS`], and the `renamed-keys` migration
/// here turns that table into the rewrite; the tests also drive the machinery
/// with a sample so the wiring is proven rather than assumed.
pub struct Migration {
    /// A short stable name, shown in the report and in `--json`.
    pub name: &'static str,
    /// What it changes and why, in one line.
    pub description: &'static str,
    /// Whether this config needs it.
    ///
    /// Gets the parsed [`Config`] *and* the raw document, because the two see
    /// different things: a key serde no longer reads vanishes from the parsed
    /// value entirely, and a key that is still read but now means something
    /// else is only visible there.
    pub applies: fn(&Config, &toml::Table) -> bool,
    /// Make the change, returning one line per thing it did. Gets the raw
    /// document for the same reason `applies` does: a key that vanished from
    /// the parsed value is still there to be named and quoted.
    pub apply: fn(&mut Config, &toml::Table) -> Vec<String>,
}

/// Why `serves = []` is worth a migration at all.
///
/// It never meant anything. `serves` is the model list a script provider with no
/// `list_models` falls back to, and an empty one is the same as no entry - so the
/// line has always been inert. It got written because the field serialized even
/// when empty, and a save-back writes every field, so `lev setup` and every
/// config migration stamped it into each `[model_providers.*]` block.
///
/// Inert is not harmless once somebody is debugging. It reads as a declaration
/// that the provider serves nothing, which is exactly what a provider whose
/// `list_models` was never asked looks like from outside - so it got the blame
/// for a routing failure it had no part in. The real fault was priming, fixed
/// separately; this removes the thing that pointed at the wrong culprit.
///
/// The migrations this build knows about, oldest first.
///
/// Adding one is adding an entry here. A key that only changed name goes in
/// [`RENAMED_KEYS`] instead, which the `renamed-keys` entry below reads.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        name: "stale-empty-serves",
        description: "remove `serves = []` from [model_providers.*] - it never meant anything",
        applies: |config, _raw| {
            config
                .model_providers
                .values()
                .any(|p| p.serves.as_ref().is_some_and(Vec::is_empty))
        },
        apply: |config, _raw| {
            let mut done = Vec::new();
            for (name, provider) in &mut config.model_providers {
                if provider.serves.as_ref().is_some_and(Vec::is_empty) {
                    provider.serves = None;
                    done.push(format!(
                        "removed empty `serves` from [model_providers.{name}]"
                    ));
                }
            }
            done
        },
    },
    // The loader already read each old key under its new name, so the parsed
    // config is right and the save writes it right; what this adds is the
    // moment where the user sees it happen, key by key, with what it means.
    Migration {
        name: "renamed-keys",
        description: "rewrite config keys that changed name (`default_model` is now `fallback_model`)",
        applies: |_config, raw| !legacy_keys_present(raw).is_empty(),
        apply: |_config, raw| {
            legacy_keys_present(raw)
                .iter()
                .map(|r| {
                    format!(
                        "`{old} = {value}` becomes `{new} = {value}`. {note}",
                        old = r.key.old,
                        new = r.key.new,
                        value = r.value,
                        note = r.key.note,
                    )
                })
                .collect()
        },
    },
];

/// What the plan found when it read the config file.
///
/// One value rather than a `Config` and an error beside it, because those two
/// only ever come in two of the four combinations and the other two would be
/// arms nothing could reach.
///
/// The config is carried rather than re-read at write time so there is exactly
/// one read: re-opening the file would add an error arm only a race could take,
/// and applying a migration to a document nobody has looked at since the report
/// was printed is exactly the surprise this command exists to avoid.
pub(crate) enum ConfigState {
    /// The config as it stands and the document behind it, for the migrations
    /// to be applied to. Boxed because a `Config` is far larger than the
    /// message beside it.
    Loaded(Box<LoadedConfig>),
    /// It could not be read, and this is why.
    Unreadable(String),
}

/// The config as `lev update` needs to see it: parsed, and the document behind
/// it.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoadedConfig {
    pub(crate) config: Config,
    pub(crate) raw: toml::Table,
}

/// Read the config file both ways.
pub(super) fn load_config(path: &Path) -> anyhow::Result<LoadedConfig> {
    let config = Config::load_from_path_public(path)?;
    let raw = match std::fs::read_to_string(path) {
        // `expect`: `load_from_path_public` above parsed this same text as
        // TOML, so a document that reaches here is a document that parses.
        Ok(text) => toml::from_str::<toml::Table>(&text).expect("the config parsed a moment ago"),
        // No file at all, which loads as the defaults and an empty document.
        Err(_) => toml::Table::new(),
    };
    Ok(LoadedConfig { config, raw })
}

/// Step three: the config.
///
/// Nothing is written before the user has seen, line by line, what changed.
pub(super) fn migrate_config(
    args: &UpdateArgs,
    env: &UpdateEnv,
    plan: &UpdatePlan,
) -> anyhow::Result<()> {
    let loaded = match &plan.config {
        ConfigState::Unreadable(e) => {
            println!("  the config could not be read, so it was left alone: {e}");
            return Ok(());
        }
        ConfigState::Loaded(loaded) => loaded,
    };
    if plan.migrations.is_empty() {
        println!("  the config needs no changes");
        return Ok(());
    }

    let mut config = loaded.config.clone();
    let mut changed = Vec::new();
    for migration in &plan.migrations {
        for line in (migration.apply)(&mut config, &loaded.raw) {
            changed.push(format!("{}: {line}", migration.name));
        }
    }
    for line in &changed {
        println!("    - {line}");
    }

    let path = env.config_path.display();
    if !agreed(args, env, &format!("Write these changes to {path}?")) {
        println!("  config left as it is");
        return Ok(());
    }
    if args.dry_run {
        println!("  would write {path}");
        return Ok(());
    }
    config.save_to_path_public(&env.config_path)?;
    println!("  wrote {path}");
    Ok(())
}
