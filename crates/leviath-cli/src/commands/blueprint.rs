//! `lev blueprint`: work on blueprint files.
//!
//! `lev blueprint migrate` converts an `agent.leviath` manifest into an
//! `agent.toml` describing exactly the same run. Validate the result with
//! `lev validate agent.toml`. An `agent.leviath` is read by the
//! `leviath-legacy-runs` crate, so a build without its `legacy-runs` feature
//! refuses the command.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};

#[cfg(feature = "legacy-runs")]
use leviath_legacy_runs::Migrated;

/// What a build without the old-format reader would have converted to; it
/// never converts, so none is ever made.
#[cfg(not(feature = "legacy-runs"))]
pub(crate) struct Migrated {
    name: String,
    text: String,
    notes: Vec<String>,
    dropped: Vec<String>,
}

/// Arguments for `lev blueprint`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct BlueprintArgs {
    /// What to do.
    #[command(subcommand)]
    pub command: BlueprintCommand,
}

/// The `lev blueprint` subcommands.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum BlueprintCommand {
    /// Convert an `agent.leviath` manifest into an `agent.toml` blueprint
    Migrate(MigrateArgs),
}

/// Arguments for `lev blueprint migrate`.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrateArgs {
    /// The manifest: an `agent.leviath`, or the directory holding one.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Write the blueprint here instead of printing it. An existing file is
    /// left alone unless `--force` is given.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Overwrite the `--output` file when it already exists.
    #[arg(long)]
    pub force: bool,
}

/// Run `lev blueprint`.
pub async fn execute(args: BlueprintArgs) -> anyhow::Result<()> {
    match args.command {
        BlueprintCommand::Migrate(args) => {
            if let Some(said) = migrate(&args)? {
                println!("{said}");
            }
            Ok(())
        }
    }
}

/// Convert the manifest `args` names. Returns what to print: the blueprint
/// itself, or with `--output` nothing (the file is written and a line on
/// stderr says where).
pub(crate) fn migrate(args: &MigrateArgs) -> anyhow::Result<Option<String>> {
    let manifest = manifest_path(&args.path);
    let text = std::fs::read_to_string(&manifest)
        .with_context(|| format!("could not read '{}'", manifest.display()))?;
    let Migrated {
        name,
        text: blueprint,
        notes,
        dropped,
    } = convert(&text).map_err(|problems| {
        let mut lines = vec![format!(
            "'{}' does not convert: {} problem(s)",
            manifest.display(),
            problems.len()
        )];
        lines.extend(problems.iter().map(|p| format!("  {p}")));
        anyhow::anyhow!(lines.join("\n"))
    })?;
    for line in said(&name, &notes, &dropped) {
        eprintln!("{line}");
    }
    let Some(out) = &args.output else {
        return Ok(Some(blueprint));
    };
    if out.exists() && !args.force {
        bail!(
            "'{}' already exists; pass --force to overwrite it",
            out.display()
        );
    }
    std::fs::write(out, &blueprint)
        .with_context(|| format!("could not write '{}'", out.display()))?;
    eprintln!(
        "wrote {}; check it with `lev validate {}`",
        out.display(),
        out.display()
    );
    Ok(None)
}

/// What a migration tells the person running it besides the blueprint: a
/// `note:` for each setting the new file spells differently, and a
/// `warning:` for each key it dropped because the old release never read it.
fn said(name: &str, notes: &[String], dropped: &[impl std::fmt::Display]) -> Vec<String> {
    let notes = notes.iter().map(|note| format!("note: {note}"));
    let dropped = dropped
        .iter()
        .map(|line| format!("warning: blueprint '{name}': {line}"));
    notes.chain(dropped).collect()
}

/// The name of an `agent.leviath` manifest, inside its directory.
const MANIFEST_FILE: &str = "agent.leviath";

/// The manifest `path` names: itself, or the `agent.leviath` in it.
fn manifest_path(path: &Path) -> PathBuf {
    match path.is_dir() {
        true => path.join(MANIFEST_FILE),
        false => path.to_path_buf(),
    }
}

/// The text of an `agent.leviath` as an `agent.toml`, or every problem with
/// it.
#[cfg(feature = "legacy-runs")]
pub(crate) fn convert(manifest: &str) -> Result<Migrated, Vec<String>> {
    leviath_legacy_runs::migrate_noted(manifest)
}

/// Without the old-format reader there is nothing to convert with.
#[cfg(not(feature = "legacy-runs"))]
pub(crate) fn convert(_manifest: &str) -> Result<Migrated, Vec<String>> {
    Err(vec![
        "this build of lev cannot read agent.leviath files (it was built without the \
         legacy-runs feature)"
            .to_string(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A coder-shaped `agent.leviath` manifest.
    const OLD_CODER: &str = r#"[agent]
name = "coder"
version = "0.0.0"
description = "A coder-shaped manifest in the old format."
entry_stage = "analyze"

[stages.analyze]
mode = "autonomous"
model = { provider = "anthropic", model = "m" }
available_tools = ["read_file", "list_dir"]
system_prompt = "Analyze the task."
[stages.analyze.transitions.review]
transform = "compact"

[stages.review]
mode = "autonomous"
model = { provider = "anthropic", model = "m" }
available_tools = ["read_file"]
system_prompt = "Review."

[context.regions]
task = { kind = "pinned", max_tokens = 2000, seed = "task" }
conversation = { kind = "sliding_window", max_items = 40, max_tokens = 20000 }
"#;

    fn manifest(dir: &Path) -> PathBuf {
        let path = dir.join(MANIFEST_FILE);
        std::fs::write(&path, OLD_CODER).unwrap();
        path
    }

    /// A fan-out a manifest gave `max_workers = 0` (no cap) has no
    /// `max_workers` in its blueprint, which is how a graph says no cap, and
    /// the conversion says so.
    #[tokio::test]
    async fn a_fan_out_with_no_cap_converts_to_one_that_leaves_it_out() {
        let dir = tempfile::tempdir().unwrap();
        let text = OLD_CODER.replace(
            "[stages.review]\nmode = \"autonomous\"",
            "[stages.review]\nmode = \"fan_out\"\nworker_agent = \"helper\"\nmax_workers = 0",
        );
        assert_ne!(text, OLD_CODER);
        std::fs::write(dir.path().join(MANIFEST_FILE), text).unwrap();
        let printed = migrate(&MigrateArgs {
            path: dir.path().to_path_buf(),
            ..Default::default()
        })
        .unwrap()
        .expect("printed");
        assert!(printed.contains("fan_out"), "{printed}");
        assert!(!printed.contains("max_workers"), "{printed}");
    }

    /// A key the old release accepted and never read is left out of the
    /// blueprint, and the migration warns about it, naming the blueprint, the
    /// key and its value.
    #[tokio::test]
    async fn a_key_nothing_read_is_dropped_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let text = OLD_CODER.replacen("[agent]", "[agent]\ncolour = \"blue\"", 1);
        std::fs::write(dir.path().join(MANIFEST_FILE), &text).unwrap();
        let printed = migrate(&MigrateArgs {
            path: dir.path().to_path_buf(),
            ..Default::default()
        })
        .unwrap()
        .expect("printed");
        assert!(!printed.contains("colour"), "{printed}");
        let done = convert(&text).unwrap();
        let lines = said(&done.name, &["spelled anew".to_string()], &done.dropped);
        assert_eq!(lines[0], "note: spelled anew");
        assert!(
            lines[1].starts_with(
                "warning: blueprint 'coder': [agent]: `colour = \"blue\"` was dropped: Leviath \
                 0.6.4 and earlier accepted it but never read it"
            ),
            "{lines:?}"
        );
    }

    /// A manifest converts to a blueprint that loads as the same run, named
    /// by the file or by its directory, printed or written.
    #[tokio::test]
    async fn a_manifest_converts_to_a_blueprint_that_validates() {
        let dir = tempfile::tempdir().unwrap();
        manifest(dir.path());
        let printed = migrate(&MigrateArgs {
            path: dir.path().to_path_buf(),
            ..Default::default()
        })
        .unwrap()
        .expect("printed");
        assert!(printed.contains("[blueprint]"), "{printed}");
        assert!(printed.contains("name = \"coder\""), "{printed}");

        let out = dir.path().join(leviath_blueprint::FILE_NAME);
        let args = MigrateArgs {
            path: dir.path().join(MANIFEST_FILE),
            output: Some(out.clone()),
            force: false,
        };
        assert!(migrate(&args).unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), printed);
        let loaded = leviath_blueprint::validate(&out).expect("the blueprint validates");
        assert_eq!(loaded.reference.name.as_str(), "coder");

        // A second write is refused unless forced.
        let err = migrate(&args).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        assert!(
            migrate(&MigrateArgs {
                force: true,
                ..args.clone()
            })
            .unwrap()
            .is_none()
        );
        execute(BlueprintArgs {
            command: BlueprintCommand::Migrate(MigrateArgs {
                force: true,
                ..args
            }),
        })
        .await
        .unwrap();
        // Printed rather than written, and a manifest that is not there.
        execute(BlueprintArgs {
            command: BlueprintCommand::Migrate(MigrateArgs {
                path: dir.path().to_path_buf(),
                ..Default::default()
            }),
        })
        .await
        .unwrap();
        assert!(
            execute(BlueprintArgs {
                command: BlueprintCommand::Migrate(MigrateArgs {
                    path: dir.path().join("gone"),
                    ..Default::default()
                }),
            })
            .await
            .is_err()
        );
    }

    /// Every problem a manifest has is listed, one per line; a manifest that
    /// is not there, or a file that cannot be written, says which.
    #[test]
    fn a_manifest_that_does_not_convert_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.leviath");
        std::fs::write(&bad, "this is : not = valid toml [[[").unwrap();
        let err = migrate(&MigrateArgs {
            path: bad,
            ..Default::default()
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not convert: 1 problem(s)"), "{err}");

        let err = migrate(&MigrateArgs {
            path: dir.path().join("missing"),
            ..Default::default()
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("could not read"), "{err}");

        manifest(dir.path());
        let err = migrate(&MigrateArgs {
            path: dir.path().to_path_buf(),
            output: Some(dir.path().join("no-such-dir").join("agent.toml")),
            force: false,
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("could not write"), "{err}");
    }
}
