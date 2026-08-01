//! `lev run` - run an agent in the shared-world daemon.
//!
//! `run` resolves the blueprint + task locally and asks the running daemon (auto-
//! started if needed) to create the agent in the one shared ECS world. The
//! request-building + daemon exchange live in [`crate::daemon::client`]; this
//! module keeps the manifest/session/tool-source helpers still shared across the
//! CLI, and the `RunArgs` the binary wires into that path.

pub mod manifest;
pub mod session;

use std::collections::HashMap;

use clap::Args;

// Re-export the provider-registry builders used by the daemon setup.
pub use session::{
    ProviderCreds, build_provider_registry, build_provider_registry_from_config,
    provider_creds_from_config,
};

/// Arguments for `lev run`.
#[derive(Args, Debug, Clone, Default)]
pub struct RunArgs {
    /// Path to the agent (a manifest file, its directory, or an installed name).
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Task prompt for the agent.
    #[arg(short, long)]
    pub task: Option<String>,

    /// Model override (`provider/model` or a bare model name).
    #[arg(short, long)]
    pub model: Option<String>,

    /// Run unattended: approve every tool call, and answer the agent's own
    /// prompts (ask_user_*, plan approvals) instead of waiting for a person.
    #[arg(long)]
    pub yolo: bool,

    /// Allow a tool outright (repeatable).
    #[arg(long)]
    pub allow: Vec<String>,

    /// Override the blueprint's max sub-agent tree depth.
    #[arg(long)]
    pub max_depth: Option<usize>,

    /// Refuse the blueprint's `seed = { command = "..." }` regions. Those run a
    /// shell command at spawn - before the first inference, and so before any
    /// approval prompt. See `lev validate <path>` to inspect them first.
    #[arg(long)]
    pub no_seed_commands: bool,

    /// Working directory for the run (default: the directory `lev run` is
    /// invoked from). The agent's file tools are confined to it, and relative
    /// `[read_paths]` entries resolve against it.
    #[arg(long, value_name = "DIR")]
    pub workdir: Option<std::path::PathBuf>,

    /// Dynamic per-region seed flags (`--<region> <text|@file>`), collected by an
    /// argv pre-scan in the binary since region names are blueprint-defined.
    /// clap skips this field; it is populated after parsing.
    #[arg(skip)]
    pub regions: HashMap<String, String>,
}

/// The `run` subcommand's own long flags - everything NOT in this set is treated
/// as a dynamic `--<region>` seed flag by [`extract_region_flags`].
const KNOWN_RUN_FLAGS: &[&str] = &[
    "task",
    "model",
    "yolo",
    "allow",
    "max-depth",
    "no-seed-commands",
    "workdir",
    "verbose",
    "help",
    "version",
];

/// The run's effective working directory: the `--workdir` flag when given
/// (canonicalized, and refused early when it does not exist - a bad workdir
/// would otherwise spawn an agent whose every tool call fails), else `cwd`
/// (the directory the command was invoked from, resolved by the caller).
pub fn effective_workdir(
    flag: Option<std::path::PathBuf>,
    cwd: std::path::PathBuf,
) -> anyhow::Result<String> {
    let dir = match flag {
        Some(dir) => {
            let canonical = std::fs::canonicalize(&dir).map_err(|e| {
                anyhow::anyhow!(
                    "--workdir '{}' is not a usable directory: {e}",
                    dir.display()
                )
            })?;
            if !canonical.is_dir() {
                anyhow::bail!("--workdir '{}' is not a directory", dir.display());
            }
            canonical
        }
        None => cwd,
    };
    Ok(dir.to_string_lossy().to_string())
}

/// Pre-scan a full argv (program name first) for dynamic `--<region>` flags on
/// the `run` subcommand, since region names are blueprint-defined and clap can't
/// declare them. Returns `(argv_for_clap, region_flags)`: a `--<name>` (or
/// `--<name>=<value>`) whose `<name>` is not a known `run` flag is pulled out
/// (with its value) into the map; every other token passes through untouched.
///
/// A no-`=` region flag consumes the following token as its value. If argv has
/// no `run` subcommand token, nothing is extracted (the returned argv equals the
/// input). Pure - no environment or I/O - so it is unit-testable in isolation.
pub fn extract_region_flags(argv: Vec<String>) -> (Vec<String>, HashMap<String, String>) {
    // Locate the subcommand: the first bareword (non-`-`) token after the program
    // name. Only activate when it is `run`.
    let sub_pos = argv
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, t)| !t.starts_with('-'))
        .map(|(i, _)| i);
    let Some(sub_pos) = sub_pos else {
        return (argv, HashMap::new());
    };
    if argv[sub_pos] != "run" {
        return (argv, HashMap::new());
    }

    let mut out: Vec<String> = argv[..=sub_pos].to_vec();
    let mut regions = HashMap::new();
    let mut i = sub_pos + 1;
    while i < argv.len() {
        let token = &argv[i];
        if let Some(name) = token.strip_prefix("--") {
            // Split an `=`-joined value if present.
            let (name, inline) = match name.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (name, None),
            };
            if !name.is_empty() && !KNOWN_RUN_FLAGS.contains(&name) {
                let value = match inline {
                    Some(v) => v,
                    None => {
                        // Consume the next token as the value, if any.
                        i += 1;
                        argv.get(i).cloned().unwrap_or_default()
                    }
                };
                regions.insert(name.to_string(), value);
                i += 1;
                continue;
            }
        }
        out.push(token.clone());
        i += 1;
    }
    (out, regions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn extracts_dynamic_region_flags_and_preserves_known_ones() {
        let (out, regions) = extract_region_flags(argv(&[
            "lev",
            "run",
            "agents/reviewer",
            "--task",
            "review it",
            "--files",
            "@src/main.rs",
            "--review-criteria",
            "@policy.md",
            "--yolo",
        ]));
        // Known flags + positional pass through to clap.
        assert_eq!(
            out,
            argv(&[
                "lev",
                "run",
                "agents/reviewer",
                "--task",
                "review it",
                "--yolo",
            ])
        );
        assert_eq!(
            regions.get("files").map(String::as_str),
            Some("@src/main.rs")
        );
        assert_eq!(
            regions.get("review-criteria").map(String::as_str),
            Some("@policy.md")
        );
    }

    #[test]
    fn extracts_equals_joined_region_flag() {
        let (out, regions) = extract_region_flags(argv(&["lev", "run", "a", "--criteria=be safe"]));
        assert_eq!(out, argv(&["lev", "run", "a"]));
        assert_eq!(regions.get("criteria").map(String::as_str), Some("be safe"));
    }

    #[test]
    fn no_region_flags_leaves_argv_unchanged() {
        let input = argv(&["lev", "run", "a", "--task", "t"]);
        let (out, regions) = extract_region_flags(input.clone());
        assert_eq!(out, input);
        assert!(regions.is_empty());
    }

    #[test]
    fn non_run_subcommand_is_untouched() {
        // A dynamic-looking flag on another subcommand is left for clap to reject.
        let input = argv(&["lev", "ps", "--weird", "x"]);
        let (out, regions) = extract_region_flags(input.clone());
        assert_eq!(out, input);
        assert!(regions.is_empty());
    }

    #[test]
    fn no_subcommand_token_is_untouched() {
        // Only flags, no bareword subcommand → nothing extracted.
        let input = argv(&["lev", "--verbose"]);
        let (out, regions) = extract_region_flags(input.clone());
        assert_eq!(out, input);
        assert!(regions.is_empty());
    }

    #[test]
    fn trailing_region_flag_without_value_maps_to_empty() {
        // A dynamic flag at the very end with no following value → empty string.
        let (out, regions) = extract_region_flags(argv(&["lev", "run", "a", "--spec"]));
        assert_eq!(out, argv(&["lev", "run", "a"]));
        assert_eq!(regions.get("spec").map(String::as_str), Some(""));
    }

    #[test]
    fn global_verbose_before_run_still_activates() {
        let (_out, regions) = extract_region_flags(argv(&["lev", "-v", "run", "a", "--spec", "x"]));
        assert_eq!(regions.get("spec").map(String::as_str), Some("x"));
    }

    /// `--workdir` is a real run flag: the pre-scan must pass it through to
    /// clap, not swallow it as a region seed named "workdir".
    #[test]
    fn workdir_flag_is_not_eaten_as_a_region() {
        let input = argv(&["lev", "run", "a", "--workdir", "/elsewhere", "--task", "t"]);
        let (out, regions) = extract_region_flags(input.clone());
        assert_eq!(out, input);
        assert!(regions.is_empty());
    }

    #[test]
    fn effective_workdir_uses_the_flag_canonicalized() {
        let dir = tempfile::tempdir().unwrap();
        let got = effective_workdir(
            Some(dir.path().to_path_buf()),
            std::path::PathBuf::from("/unused"),
        )
        .unwrap();
        assert_eq!(
            got,
            std::fs::canonicalize(dir.path())
                .unwrap()
                .to_string_lossy()
                .to_string()
        );
    }

    #[test]
    fn effective_workdir_defaults_to_the_supplied_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let got = effective_workdir(None, cwd.path().to_path_buf()).unwrap();
        assert_eq!(got, cwd.path().to_string_lossy().to_string());
    }

    /// A bad `--workdir` fails before the daemon is contacted - otherwise it
    /// spawns an agent whose every tool call fails.
    #[test]
    fn effective_workdir_refuses_a_missing_or_non_directory_path() {
        let cwd = std::path::PathBuf::from("/unused");
        let err = effective_workdir(
            Some(std::path::PathBuf::from("/definitely/not/a/real/dir")),
            cwd.clone(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a usable directory"), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let err = effective_workdir(Some(file), cwd).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
    }
}
