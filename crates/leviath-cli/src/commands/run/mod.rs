//! `lev run` - run an agent in the shared-world daemon.
//!
//! `run` reads the command line into a spawn request locally ([`request`]),
//! each input by the type its blueprint declares (`inputs`), and asks the
//! running daemon (auto-started if needed) to start it in the one shared ECS
//! world, or with `--check` only to say what it would be ([`check`]). `lev
//! run show` reads a run's file back ([`show`]). The daemon exchange lives in
//! [`crate::daemon::client`]; this module keeps the blueprint-finding and session helpers
//! shared across the CLI, and the `RunArgs` the binary wires into that path.

pub mod attach;
pub mod check;
pub(crate) mod inputs;
pub(crate) mod locate;
pub mod request;
pub(crate) mod session;
pub mod show;
pub(crate) mod task;

use std::collections::HashMap;

use clap::Args;

// Re-export the provider-registry builders used by the daemon setup.
#[cfg(test)]
pub(crate) use session::build_provider_registry_from_config_probing;
pub(crate) use session::{
    build_provider_registry_from_config, build_provider_registry_from_config_with,
};

/// What `lev run` does besides starting a run.
#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum RunCommand {
    /// Show what a run's file holds: its spec, its state after any step, or
    /// its steps
    Show(show::ShowArgs),
}

/// Arguments for `lev run`.
#[derive(Args, Debug, Clone, Default)]
#[command(args_conflicts_with_subcommands = true)]
pub struct RunArgs {
    /// `lev run show <run>`: read a run's file instead of starting a run.
    #[command(subcommand)]
    pub command: Option<RunCommand>,

    /// Path to the agent (a manifest file, its directory, or an installed name).
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Task prompt, or the path of a file holding it: the blueprint's `task`
    /// input. Left off, your editor opens on a template for you to write it
    /// in.
    #[arg(short, long, value_name = "TEXT|FILE")]
    pub task: Option<String>,

    /// The inputs, `--request` and `--check`: everything that says what the
    /// run reads, carried as one value.
    #[command(flatten)]
    pub regions: RunInputs,

    /// Model override (`provider/model` or a bare model name).
    #[arg(short, long)]
    pub model: Option<String>,

    /// Run unattended: approve every tool call, and answer the agent's own
    /// prompts (ask_user_*, interaction points) instead of waiting for a person.
    ///
    /// One exception, and it is the one that looks like a hang: an interaction
    /// point declaring `unattended = "ask"` still holds for a person. The
    /// bundled coder's plan approval does not (it resolves as approved, so CI
    /// can run it), but a blueprint whose checkpoint guards something that
    /// cannot be undone may well set it. Such a run parks in `Waiting` until
    /// somebody answers; set `[limits] interaction_timeout_secs` to bound the
    /// wait.
    ///
    /// It also waives the taint gate. An attended run asks before an outbound
    /// tool sends data more sensitive than its clearance, and `submit_output`
    /// counts: `lev serve` hands the answer to whoever reads
    /// `GET /api/runs/{id}/result`. Unattended there is nobody to ask, so the
    /// call goes through and the override is recorded in the run's
    /// `stages/<n>/taint_audit.json` as `YoloAutoApprove`. Think twice before
    /// combining `--yolo` with an agent whose `[read_paths]` reach private
    /// files.
    ///
    /// `--yolo=<profile>` runs under a named profile from `yolo.toml` beside
    /// your config instead: the profile says which tool calls and shell
    /// commands run unprompted, which still ask, and whether the model's
    /// questions and the stage checkpoints still come to you. `lev yolo list`
    /// shows the profiles you have. The equals sign is required, so
    /// `lev run --yolo coder` keeps meaning "run coder, plain yolo".
    #[arg(
        long,
        value_name = "PROFILE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = ""
    )]
    pub yolo: Option<String>,

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
    /// `--check`, the summary (or the list of problems) is the JSON.
    #[arg(long)]
    pub json: bool,

    /// Start this many runs of the same agent and task, each under its own run
    /// id, from one invocation. One process launch and one socket dial per run
    /// caps a shell loop near 60 spawns/second; the daemon itself has no run
    /// cap, and a single invocation carrying the batch spawns as fast as the
    /// daemon accepts.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub count: usize,

    /// Ask for the final output in a particular shape, overriding whatever the
    /// blueprint declares. Any label works - `markdown`, `json`, `xml`, `a2ui`,
    /// a mime type, a house format - because nothing converts between shapes:
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

    /// Attach a file to the run: `path[:region][:type][:text]`. The file lands
    /// in the named region (default: the task region) as a typed part. `:type`
    /// names its mime type when the registry cannot tell; `:text` sends its
    /// bytes to the model as text whatever the model takes. Repeatable. A
    /// `@path` inside the task text does the same for that file.
    #[arg(long, value_name = "PATH[:REGION][:TYPE][:text]")]
    pub attach: Vec<String>,
}

/// What `lev run` reads, besides the launch flags: its inputs, a whole
/// request in place of a blueprint, whether only to check it, and which of
/// the flags the binary resolves itself were given.
///
/// One value, so the binary hands it to
/// [`LaunchRequest`](crate::daemon::client::LaunchRequest) whole and every
/// part of it is read in the library, where the tests reach it.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct RunInputs {
    /// Give the run an input, read by the type the blueprint declares for it
    /// (repeatable). Text is the text, or `@file` for a file's text; a number
    /// is a number; `true` or `false`; a list is `a,b,c` or a JSON array; a
    /// record is a JSON object; a file input takes `@path`, which is attached
    /// and named. `--<name> value` is the short form for any input.
    #[arg(short, long = "input", value_name = "NAME=VALUE")]
    pub inputs: Vec<String>,

    /// Send a whole spawn request from a file, TOML or JSON, instead of
    /// naming a blueprint. `lev schema spawn-request` prints what one holds.
    /// Any input and launch flag given beside it lands on it too.
    #[arg(long, value_name = "FILE")]
    pub request: Option<std::path::PathBuf>,

    /// Check the run without starting it: the daemon resolves it the whole
    /// way and prints what it would run (each stage's model and tools, the
    /// inputs, the workdir), or every problem it found, one per line with
    /// where in the request it is.
    #[arg(long)]
    pub check: bool,

    /// Inputs given the short way (`--<name> <value>`), collected by an argv
    /// pre-scan ([`extract_region_flags`]) since input names are
    /// blueprint-defined. clap skips this field; it is filled after parsing.
    #[arg(skip)]
    pub named: HashMap<String, String>,

    /// Whether a blueprint PATH was typed. The binary passes `.` when none
    /// was, and a `--request` file names its own blueprint.
    #[arg(skip)]
    pub path_given: bool,

    /// Whether `--workdir` was typed rather than defaulted: a `--request`
    /// file's own workdir gives way only to one asked for.
    #[arg(skip)]
    pub workdir_given: bool,
}

/// Every long flag `lev run` and `lev run show` own, and the global ones,
/// asked of clap's own `root` command. Anything else after `run` is an input
/// given the short way (`--<name> value`) by [`extract_region_flags`].
///
/// Asked of clap rather than listed by hand: a flag missing from a hand list
/// is not a parse error, it is silently read as an input and eats the token
/// after it.
pub fn known_run_flags(root: &clap::Command) -> Vec<String> {
    let longs = |command: &clap::Command| -> Vec<String> {
        command
            .get_arguments()
            .flat_map(|arg| {
                arg.get_long()
                    .into_iter()
                    .chain(arg.get_all_aliases().into_iter().flatten())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    let mut known = longs(root);
    if let Some(run) = root.find_subcommand("run") {
        known.extend(longs(run));
        known.extend(run.get_subcommands().flat_map(longs));
    }
    // clap adds these to every command as it builds one for parsing, which a
    // command read before then does not show.
    known.extend(["help".to_string(), "version".to_string()]);
    known
}

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
        overwrite_artifacts: None,
        artifacts: Vec::new(),
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

/// Pre-scan a full argv (program name first) for inputs given the short way
/// (`--<name> value`) on the `run` subcommand, against the flags of the `lev`
/// command line ([`crate::dispatch::command_line`]). See
/// [`extract_named_inputs`].
pub fn extract_region_flags(argv: Vec<String>) -> (Vec<String>, HashMap<String, String>) {
    extract_named_inputs(argv, &known_run_flags(&crate::dispatch::command_line()))
}

/// Pre-scan a full argv (program name first) for inputs given the short way
/// (`--<name> value`) on the `run` subcommand, since input names are
/// blueprint-defined and clap can't declare them. Returns `(argv_for_clap,
/// named_inputs)`: a `--<name>` (or `--<name>=<value>`) whose `<name>` is not
/// in `known` ([`known_run_flags`]) is pulled out (with its value) into the
/// map; every other token passes through untouched.
///
/// A no-`=` flag consumes the following token as its value. If argv has no
/// `run` subcommand token, or it is `lev run show`, nothing is extracted (the
/// returned argv equals the input). Pure - no environment or I/O - so it is
/// unit-testable in isolation.
pub fn extract_named_inputs(
    argv: Vec<String>,
    known: &[String],
) -> (Vec<String>, HashMap<String, String>) {
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
    if argv[sub_pos] != "run" || argv.get(sub_pos + 1).is_some_and(|t| t == "show") {
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
            if !name.is_empty() && !known.iter().any(|k| k == name) {
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
        // A flag clap owns but the pre-scan does not know is not a parse
        // error: it is read as an input, eats the token after it, and the run
        // starts with an input nobody asked for. The known set is clap's own,
        // so a new flag is in it the moment it is declared.
        let root = lev();
        let known = known_run_flags(&root);
        let run = root.find_subcommand("run").expect("run");
        let show = run.find_subcommand("show").expect("run show");
        for arg in root
            .get_arguments()
            .chain(run.get_arguments())
            .chain(show.get_arguments())
        {
            if let Some(long) = arg.get_long() {
                assert!(known.iter().any(|k| k == long), "`--{long}` is not known");
            }
        }
        for flag in [
            "verbose", "input", "request", "check", "help", "version", "at",
        ] {
            assert!(known.iter().any(|k| k == flag), "{flag}");
        }
        assert_eq!(
            known_run_flags(&clap::Command::new("bare")),
            ["help", "version"]
        );
    }

    /// The `lev` command line.
    fn lev() -> clap::Command {
        crate::dispatch::command_line()
    }

    /// The binary's own pre-scan asks the real command line which flags are
    /// `run`'s, so a global flag passes through and an input is pulled out.
    #[test]
    fn the_binary_pre_scan_knows_the_real_flags() {
        let (out, named) = extract_region_flags(argv(&[
            "lev",
            "--verbose",
            "run",
            "a",
            "--check",
            "--spec",
            "x",
        ]));
        assert_eq!(out, argv(&["lev", "--verbose", "run", "a", "--check"]));
        assert_eq!(named.get("spec").map(String::as_str), Some("x"));
    }

    /// The known flags of the real command line, for the pre-scan tests.
    fn known() -> Vec<String> {
        known_run_flags(&lev())
    }

    /// `lev run show <run>` is the subcommand, never a blueprint named
    /// `show`, and its flags are its own: the pre-scan leaves the line
    /// alone and clap reads it.
    #[test]
    fn run_show_is_a_subcommand_and_its_flags_are_not_inputs() {
        use clap::Parser as _;
        #[derive(clap::Parser)]
        struct Probe {
            #[command(flatten)]
            run: RunArgs,
        }
        let line = argv(&["lev", "run", "show", "r1", "--at", "3", "--weird", "x"]);
        let (out, regions) = extract_named_inputs(line.clone(), &known());
        assert_eq!(out, line);
        assert!(regions.is_empty());
        let p = Probe::try_parse_from(["lev", "show", "r1", "--at", "3", "--json"]).unwrap();
        assert_eq!(
            p.run.command,
            Some(RunCommand::Show(show::ShowArgs {
                run_id: "r1".to_string(),
                at: Some(3),
                json: true,
                ..Default::default()
            }))
        );
        assert!(p.run.path.is_none());
        // A blueprint path is still a path, and the new flags read.
        let p = Probe::try_parse_from([
            "lev",
            "coder",
            "-i",
            "depth=3",
            "--input",
            "tags=a,b",
            "--check",
            "--request",
            "r.toml",
        ])
        .unwrap();
        assert_eq!(p.run.path.as_deref(), Some("coder"));
        assert_eq!(p.run.regions.inputs, ["depth=3", "tags=a,b"]);
        assert!(p.run.regions.check);
        assert_eq!(
            p.run.regions.request.as_deref(),
            Some(std::path::Path::new("r.toml"))
        );
        assert!(p.run.command.is_none());
    }

    #[test]
    fn extracts_dynamic_region_flags_and_preserves_known_ones() {
        let (out, regions) = extract_named_inputs(
            argv(&[
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
            ]),
            &known(),
        );
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
        let (out, regions) =
            extract_named_inputs(argv(&["lev", "run", "a", "--criteria=be safe"]), &known());
        assert_eq!(out, argv(&["lev", "run", "a"]));
        assert_eq!(regions.get("criteria").map(String::as_str), Some("be safe"));
    }

    #[test]
    fn no_region_flags_leaves_argv_unchanged() {
        let input = argv(&["lev", "run", "a", "--task", "t"]);
        let (out, regions) = extract_named_inputs(input.clone(), &known());
        assert_eq!(out, input);
        assert!(regions.is_empty());
    }

    #[test]
    fn non_run_subcommand_is_untouched() {
        // A dynamic-looking flag on another subcommand is left for clap to reject.
        let input = argv(&["lev", "ps", "--weird", "x"]);
        let (out, regions) = extract_named_inputs(input.clone(), &known());
        assert_eq!(out, input);
        assert!(regions.is_empty());
    }

    #[test]
    fn no_subcommand_token_is_untouched() {
        // Only flags, no bareword subcommand → nothing extracted.
        let input = argv(&["lev", "--verbose"]);
        let (out, regions) = extract_named_inputs(input.clone(), &known());
        assert_eq!(out, input);
        assert!(regions.is_empty());
    }

    #[test]
    fn trailing_region_flag_without_value_maps_to_empty() {
        // A dynamic flag at the very end with no following value → empty string.
        let (out, regions) = extract_named_inputs(argv(&["lev", "run", "a", "--spec"]), &known());
        assert_eq!(out, argv(&["lev", "run", "a"]));
        assert_eq!(regions.get("spec").map(String::as_str), Some(""));
    }

    #[test]
    fn global_verbose_before_run_still_activates() {
        let (_out, regions) =
            extract_named_inputs(argv(&["lev", "-v", "run", "a", "--spec", "x"]), &known());
        assert_eq!(regions.get("spec").map(String::as_str), Some("x"));
    }

    /// `--workdir` is a real run flag: the pre-scan must pass it through to
    /// clap, not swallow it as a region seed named "workdir".
    #[test]
    fn workdir_flag_is_not_eaten_as_a_region() {
        let input = argv(&["lev", "run", "a", "--workdir", "/elsewhere", "--task", "t"]);
        let (out, regions) = extract_named_inputs(input.clone(), &known());
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

    /// `--yolo` alone is the bare flag, `--yolo=<name>` names a profile, and
    /// the space form keeps meaning "bare flag, then the agent": otherwise
    /// `lev run --yolo coder` would silently read `coder` as a profile.
    #[test]
    fn the_yolo_flag_takes_a_profile_only_with_an_equals_sign() {
        use clap::Parser as _;
        #[derive(clap::Parser)]
        struct Probe {
            #[command(flatten)]
            run: RunArgs,
        }
        let p = Probe::try_parse_from(["lev", "coder", "--yolo"]).expect("bare flag");
        assert_eq!(p.run.yolo.as_deref(), Some(""));
        assert_eq!(p.run.path.as_deref(), Some("coder"));
        let p = Probe::try_parse_from(["lev", "--yolo=careful", "coder"]).expect("named");
        assert_eq!(p.run.yolo.as_deref(), Some("careful"));
        assert_eq!(p.run.path.as_deref(), Some("coder"));
        let p = Probe::try_parse_from(["lev", "--yolo", "coder"]).expect("space form");
        assert_eq!(p.run.yolo.as_deref(), Some(""));
        assert_eq!(p.run.path.as_deref(), Some("coder"));
        let p = Probe::try_parse_from(["lev", "coder"]).expect("no flag");
        assert!(p.run.yolo.is_none());
    }

    /// The argv pre-scan leaves `--yolo=<name>` alone: `yolo` is a known flag
    /// whichever way its value is attached.
    #[test]
    fn the_pre_scan_passes_a_profile_flag_through() {
        let (out, regions) = extract_named_inputs(
            argv(&["lev", "run", "coder", "--yolo=careful", "--files", "@x"]),
            &known(),
        );
        assert_eq!(out, argv(&["lev", "run", "coder", "--yolo=careful"]));
        assert_eq!(regions.get("files").map(String::as_str), Some("@x"));
    }
}
