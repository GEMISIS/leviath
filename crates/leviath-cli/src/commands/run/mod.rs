//! `lev run` - run an agent in the shared-world daemon.
//!
//! `run` resolves the blueprint + task locally and asks the running daemon (auto-
//! started if needed) to create the agent in the one shared ECS world. The
//! request-building + daemon exchange live in [`crate::daemon::client`]; this
//! module keeps the manifest/session/tool-source helpers still shared across the
//! CLI, and the `RunArgs` the binary wires into that path.

pub(crate) mod manifest;
pub(crate) mod session;
pub(crate) mod task;

use std::collections::HashMap;

use clap::Args;

// Re-export the provider-registry builders used by the daemon setup.
#[cfg(test)]
pub(crate) use session::build_provider_registry_from_config_probing;
pub(crate) use session::{
    build_provider_registry_from_config, build_provider_registry_from_config_with,
};

/// Arguments for `lev run`.
#[derive(Args, Debug, Clone, Default)]
pub struct RunArgs {
    /// Path to the agent (a manifest file, its directory, or an installed name).
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Task prompt, or the path of a file holding it. Left off, your editor
    /// opens on a template for you to write it in.
    #[arg(short, long, value_name = "TEXT|FILE")]
    pub task: Option<String>,

    /// Model override (`provider/model` or a bare model name).
    #[arg(short, long)]
    pub model: Option<String>,

    /// Run unattended: approve every tool call, and answer the agent's own
    /// prompts (ask_user_*, interaction points) instead of waiting for a person.
    ///
    /// One exception, and it is the one that looks like a hang: an interaction
    /// point declaring `unattended = "ask"` still holds for a person. The
    /// bundled coder's plan approval can, deliberately, because
    /// everything after it writes code. Such a run parks in `Waiting` until
    /// somebody answers; set `[limits] interaction_timeout_secs` to bound the
    /// wait.
    ///
    /// It also waives the taint gate. An attended run asks before an outbound
    /// tool sends data more sensitive than its clearance, and `submit_output`
    /// counts: `lev serve` hands the answer to whoever reads
    /// `GET /api/agents/{id}/result`. Unattended there is nobody to ask, so the
    /// call goes through and the override is recorded in the run's
    /// `stages/<n>/taint_audit.json` as `YoloAutoApprove`. Think twice before
    /// combining `--yolo` with an agent whose `[read_paths]` reach private
    /// files.
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

    /// Print the spawned run as JSON instead of a sentence, for a caller that
    /// has to parse the run id back out and poll `lev ps --json`. With
    /// `--count` above 1 the JSON is an array, one object per run. With
    /// `--wait` the JSON is instead one object describing the finished run:
    /// `{run_id, status, final_output}`.
    #[arg(long)]
    pub json: bool,

    /// Stay in the foreground until the run finishes, then print its final
    /// output (what `lev result <run-id>` would print) and exit non-zero when
    /// it ended in error or was cancelled. Without it `lev run` hands the run
    /// to the daemon and returns at once. Not combinable with `--count` above 1.
    #[arg(long)]
    pub wait: bool,

    /// Start this many runs of the same agent and task, each under its own run
    /// id, from one invocation. One process launch and one socket dial per run
    /// caps a shell loop near 60 spawns/second; the daemon itself has no run
    /// cap, and a single invocation carrying the batch spawns as fast as the
    /// daemon accepts.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub count: usize,

    /// Ask for the final output in a particular shape, overriding whatever the
    /// blueprint declares. Any label works - `markdown`, `json`, `xml`, `a2ui`,
    /// a media type, a house format - because nothing converts between shapes:
    /// the label and any instructions are handed to the model, which produces
    /// the bytes. Read the answer back with `lev result <run-id>`.
    ///
    /// Naming a format the blueprint does not declare retires any Rhai
    /// validator and JSON schema it declared, since a check written for one
    /// shape says nothing about another; a warning names what was retired.
    /// Pass `--output-schema` when the new shape should still be checked.
    #[arg(long, value_name = "LABEL")]
    pub output_format: Option<String>,

    /// Extra guidance about the shape, passed to the model alongside
    /// `--output-format`. This is how an unusual format gets explained.
    #[arg(long, value_name = "TEXT")]
    pub output_instructions: Option<String>,

    /// A JSON Schema (inline, or `@path` to a file) the final output must
    /// satisfy. The only thing that ever inspects the answer's contents, and it
    /// only happens because you asked: a submission that fails is refused back
    /// to the agent to correct.
    #[arg(long, value_name = "JSON|@FILE")]
    pub output_schema: Option<String>,

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
    "output-format",
    "output-instructions",
    "output-schema",
    // Every flag `run` owns must be listed here. One that is missing is not a
    // parse error: the pre-scan silently reads it as a `--<region>` seed and
    // swallows the token after it.
    "json",
    "count",
    "wait",
    "verbose",
    "help",
    "version",
];

/// Build the caller's requested output shape from the `--output-*` flags, or
/// `None` when none were given (leaving whatever the blueprint declares).
///
/// The format label is passed through untouched and never matched against a
/// known set, which is what lets `--output-format a2ui` work without a line of
/// a2ui-specific code. Only `--output-schema` is interpreted, and only as JSON,
/// because it is the one thing the runtime will actually check.
pub fn output_request(
    format: Option<String>,
    instructions: Option<String>,
    schema: Option<String>,
) -> anyhow::Result<Option<leviath_core::output::OutputSpec>> {
    if format.is_none() && instructions.is_none() && schema.is_none() {
        return Ok(None);
    }
    let schema = match schema {
        Some(raw) => {
            let text = task::read_region_value(&raw)?;
            Some(
                serde_json::from_str(&text)
                    .map_err(|e| anyhow::anyhow!("--output-schema is not valid JSON: {e}"))?,
            )
        }
        None => None,
    };
    Ok(Some(leviath_core::output::OutputSpec {
        format,
        instructions,
        example: None,
        schema,
        validator: None,
        on_validator_error: None,
    }))
}

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

    /// The format label is passed through untouched and never matched against a
    /// known set, which is what lets `--output-format a2ui` work with no
    /// a2ui-specific code anywhere.
    #[test]
    fn an_output_request_carries_an_unrecognized_format_through() {
        let spec = output_request(
            Some("a2ui".to_string()),
            Some("One card per finding.".to_string()),
            None,
        )
        .expect("no schema to parse")
        .expect("something was asked for");

        assert_eq!(spec.format.as_deref(), Some("a2ui"));
        assert_eq!(spec.instructions.as_deref(), Some("One card per finding."));
        assert!(spec.schema.is_none());
    }

    /// `--output-schema @path` reads the schema from a file, which is how a
    /// schema of any real size gets onto a command line at all. A path that is
    /// not there fails here rather than at the end of the run.
    #[test]
    fn an_output_schema_can_be_read_from_a_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("schema.json");
        std::fs::write(&path, r#"{"type":"object","required":["summary"]}"#).expect("write");

        let spec = output_request(None, None, Some(format!("@{}", path.display())))
            .expect("the file parses")
            .expect("something was asked for");
        assert_eq!(
            spec.schema,
            Some(serde_json::json!({"type": "object", "required": ["summary"]}))
        );

        let err = output_request(
            None,
            None,
            Some(format!("@{}", dir.path().join("gone.json").display())),
        )
        .expect_err("a file that is not there");
        assert!(
            err.to_string().contains("Failed to read region file"),
            "{err}"
        );
    }

    /// Nothing asked for is nothing requested, so the blueprint's own declared
    /// shape is what applies.
    #[test]
    fn no_output_flags_request_nothing() {
        assert!(
            output_request(None, None, None)
                .expect("nothing to parse")
                .is_none()
        );
    }

    /// The schema is the one flag that is interpreted, because it is the one
    /// thing the runtime will actually check. Bad JSON has to fail here, at the
    /// command line, rather than at the end of a long run.
    #[test]
    fn an_output_schema_is_parsed_and_a_broken_one_is_refused() {
        let spec = output_request(None, None, Some(r#"{"type":"object"}"#.to_string()))
            .expect("valid JSON")
            .expect("something was asked for");
        assert_eq!(spec.schema, Some(serde_json::json!({"type": "object"})));

        let err = output_request(None, None, Some("{not json".to_string()))
            .expect_err("broken JSON is refused");
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn every_flag_run_declares_is_a_known_run_flag() {
        // A flag clap owns but this list omits is not a parse error. The
        // pre-scan reads it as a `--<region>` seed, eats the token after it, and
        // the run starts with a region nobody asked for. Ask clap for the list
        // rather than repeating it, so a new flag is covered when it is added.
        let command = <RunArgs as clap::Args>::augment_args(clap::Command::new("run"));
        for arg in command.get_arguments() {
            if let Some(long) = arg.get_long() {
                assert!(
                    KNOWN_RUN_FLAGS.contains(&long),
                    "`--{long}` is missing from KNOWN_RUN_FLAGS",
                );
            }
        }
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
