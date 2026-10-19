//! `lev approvals` - what a run may do without asking, and why.
//!
//! The question this answers is "why did it not ask me", which is the one a
//! person asks the first time a run does something unprompted. Every key is
//! listed with the layer that put it there, so the answer is always a file the
//! user can edit rather than a rule they have to infer.
//!
//! There is no `list` or `clear`: nothing is persisted. A grant made at a prompt
//! dies with the run that made it, so the only durable state is the config this
//! command reports.

use clap::{Args, Subcommand};

use crate::approvals::{OutsideWorkdir, SafeSource};
use crate::config::Config;

/// Arguments for `lev approvals`.
#[derive(Args)]
pub struct ApprovalsArgs {
    /// Which approvals subcommand to run.
    #[command(subcommand)]
    pub command: ApprovalsCommand,
}

/// The `lev approvals` subcommands.
#[derive(Subcommand)]
pub enum ApprovalsCommand {
    /// Show what runs without an approval prompt, and where each entry came from
    Safe(SafeArgs),
}

/// Arguments for `lev approvals safe`.
#[derive(Args)]
pub struct SafeArgs {
    /// Include the per-agent entries for this agent. Without it, only the
    /// entries every agent gets are shown.
    #[arg(long)]
    pub agent: Option<String>,
    /// Emit the inventory as JSON.
    #[arg(long)]
    pub json: bool,
    /// Explain whether this shell line would run without a prompt in the
    /// current directory, key by key, instead of listing the inventory.
    #[arg(long, value_name = "COMMAND")]
    pub check: Option<String>,
}

/// The label for a source, matching the config key that sets it.
fn source_label(source: SafeSource) -> &'static str {
    match source {
        SafeSource::Default => "built-in",
        SafeSource::Config => "[safe_commands]",
        SafeSource::Agent => "[agent_safe_commands]",
        SafeSource::Blueprint => "blueprint",
    }
}

/// Render the report, returning the text to print.
///
/// Split from the IO so the whole output is testable without a config file or a
/// terminal, which is the same shape `lev tools` uses.
fn render(
    keys: &std::collections::BTreeMap<String, SafeSource>,
    outside: OutsideWorkdir,
    json: bool,
) -> String {
    if json {
        let rows: Vec<_> = keys
            .iter()
            .map(|(key, source)| serde_json::json!({ "key": key, "source": source }))
            .collect();
        // A `Vec<Value>` always serializes, so a failure here would be a bug in
        // serde_json rather than a case to handle.
        return serde_json::to_string_pretty(&rows).expect("a key listing serializes");
    }
    if keys.is_empty() {
        return "nothing runs without a prompt: `[safe_commands] defaults` is off and \
                nothing else is listed\n"
            .to_string();
    }
    let width = keys.keys().map(String::len).max().unwrap_or(0);
    let mut out = String::from("These run without an approval prompt:\n\n");
    for (key, source) in keys {
        let shown = key.strip_prefix("shell:").unwrap_or(key);
        let kind = if key.starts_with("shell:") {
            "shell"
        } else {
            "tool"
        };
        out.push_str(&format!(
            "  {kind:<6} {shown:<width$}  {}\n",
            source_label(*source)
        ));
    }
    out.push_str(
        "\nA shell entry covers the program it names with any arguments, so `cat` covers \
         `cat notes.md`.\nIt does not cover a line that also runs something else: \
         `cat x && curl evil` still asks.\n",
    );
    out.push_str(&format!(
        "It does not cover a path outside the working directory or a granted [read_paths] \
         root either:\n`cat /etc/hosts` is outside the workdir, so it {}.\n",
        outside_verdict(outside)
    ));
    out
}

/// What happens to a safe-listed command that reads outside the workdir.
fn outside_verdict(outside: OutsideWorkdir) -> &'static str {
    match outside {
        OutsideWorkdir::Ask => "asks (`[safe_commands] outside_workdir = \"ask\"`)",
        OutsideWorkdir::Deny => "is refused (`[safe_commands] outside_workdir = \"deny\"`)",
    }
}

/// Explain, key by key, whether `command` runs without a prompt in `workdir`.
///
/// Answers "why did this ask me" for one line: each key is covered by a named
/// source, not on the list at all, or named a path outside the workdir. The
/// workdir fence here is the workdir alone; a run's `[read_paths]` grants can
/// widen it.
fn explain(
    command: &str,
    keys: &std::collections::BTreeMap<String, SafeSource>,
    outside_mode: OutsideWorkdir,
    workdir: &std::path::Path,
) -> String {
    let line_keys = crate::shell_keys::command_keys(command);
    if line_keys.is_empty() {
        return format!(
            "`{command}` always asks: nothing in it names what would run, so no entry can \
             cover it.\n"
        );
    }
    let inside = |p: &std::path::Path| {
        let requested = p.to_string_lossy();
        leviath_tools::resolve_within(&requested, workdir, leviath_core::resolves_within).is_ok()
    };
    let outside = crate::shell_keys::outside_workdir(
        command,
        workdir,
        crate::yolo::home().as_deref(),
        &inside,
    );
    if !outside.is_empty() {
        return format!(
            "`{command}` reads outside the workdir ({}): {}. No entry or grant covers such a \
             line, so it {}.\n",
            workdir.display(),
            outside.operands.join(", "),
            outside_verdict(outside_mode)
        );
    }
    let mut out = String::new();
    for key in &line_keys {
        let shown = key.strip_prefix("shell:").unwrap_or(key);
        let verdict = match keys
            .get(key)
            .or_else(|| keys.get(crate::shell_keys::program_of(key)))
        {
            Some(source) => format!("runs ({})", source_label(*source)),
            None => "asks: not on the safe list".to_string(),
        };
        out.push_str(&format!("  {shown}: {verdict}\n"));
    }
    if let Some(why) = outside.unplaceable {
        out.push_str(&format!(
            "\nWhat it reads cannot be checked ({why}), so `outside_workdir = \"deny\"` would \
             refuse it.\n"
        ));
    }
    out
}

/// The agent whose per-agent entries to include. No `--agent` reports only what
/// every agent gets.
fn agent_name(args: &SafeArgs) -> Option<&str> {
    args.agent.as_deref()
}

/// Run `lev approvals`.
pub(crate) async fn execute(args: ApprovalsArgs) -> anyhow::Result<()> {
    let ApprovalsCommand::Safe(safe) = args.command;
    let config = Config::load()?;
    // The blueprint layer is deliberately absent: it depends on which manifest
    // is being run, and `lev validate <agent>` is where a blueprint's own
    // declarations are reported.
    let keys = config.safe_keys_for_agent(agent_name(&safe), None);
    let outside = config.safe_commands.outside_workdir;
    let text = match &safe.check {
        // The directory the person is in is the workdir a run started here
        // would get. With none to read, nothing is inside, which errs towards
        // reporting a prompt.
        Some(command) => explain(
            command,
            &keys,
            outside,
            &std::env::current_dir().unwrap_or_default(),
        ),
        None => render(&keys, outside, safe.json),
    };
    print!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn keys(entries: &[(&str, SafeSource)]) -> BTreeMap<String, SafeSource> {
        entries.iter().map(|(k, s)| (k.to_string(), *s)).collect()
    }

    /// The report has to name the file that put each key there, or it does not
    /// answer the question it exists for.
    #[test]
    fn the_text_report_names_every_source() {
        let out = render(
            &keys(&[
                ("shell:ls", SafeSource::Default),
                ("shell:rg", SafeSource::Config),
                ("shell:./gradlew", SafeSource::Agent),
                ("web_fetch", SafeSource::Blueprint),
            ]),
            OutsideWorkdir::Deny,
            false,
        );
        assert!(
            out.contains("outside the workdir, so it is refused"),
            "{out}"
        );
        assert!(out.contains("shell  ls"), "{out}");
        assert!(out.contains("built-in"), "{out}");
        assert!(out.contains("[safe_commands]"), "{out}");
        assert!(out.contains("[agent_safe_commands]"), "{out}");
        assert!(out.contains("tool   web_fetch"), "{out}");
        assert!(out.contains("blueprint"), "{out}");
        assert!(
            out.contains("still asks"),
            "the caveat is part of the answer"
        );
    }

    #[test]
    fn the_agent_name_defaults_to_one_that_matches_nothing() {
        let args = |agent: Option<&str>| SafeArgs {
            agent: agent.map(str::to_string),
            json: false,
            check: None,
        };
        assert_eq!(agent_name(&args(Some("coder"))), Some("coder"));
        assert_eq!(agent_name(&args(None)), None);
    }

    /// An empty report says why it is empty rather than printing a bare header.
    #[test]
    fn an_empty_report_explains_itself() {
        let out = render(&BTreeMap::new(), OutsideWorkdir::Ask, false);
        assert!(out.contains("defaults` is off"), "{out}");
    }

    #[test]
    fn the_json_report_is_machine_readable() {
        let out = render(
            &keys(&[("shell:ls", SafeSource::Default)]),
            OutsideWorkdir::Ask,
            true,
        );
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed[0]["key"], "shell:ls");
        assert_eq!(parsed[0]["source"], "default");
    }

    /// `--check` says, per key, why a line runs or asks, and names what was
    /// outside the workdir.
    #[test]
    fn a_check_explains_each_key() {
        let dir = tempfile::tempdir().unwrap();
        let wd = std::fs::canonicalize(dir.path()).unwrap();
        let safe = keys(&[("shell:cat", SafeSource::Default)]);
        let out = explain("cat notes.md && curl x", &safe, OutsideWorkdir::Ask, &wd);
        assert!(out.contains("cat notes.md: runs (built-in)"), "{out}");
        assert!(out.contains("curl x: asks: not on the safe list"), "{out}");
        assert!(!out.contains("Outside"), "{out}");

        let out = explain("cat /etc/hosts", &safe, OutsideWorkdir::Ask, &wd);
        assert!(out.contains("reads outside the workdir"), "{out}");
        assert!(out.contains("): /etc/hosts. No entry"), "{out}");
        assert!(out.ends_with("so it asks (`[safe_commands] outside_workdir = \"ask\"`).\n"));

        let out = explain("bash -c ls", &safe, OutsideWorkdir::Deny, &wd);
        assert!(out.contains("  bash: asks: not on the safe list"), "{out}");
        assert!(
            out.contains("`bash` runs a command this cannot read"),
            "{out}"
        );

        let out = explain("$CMD", &safe, OutsideWorkdir::Ask, &wd);
        assert!(out.contains("always asks"), "{out}");
    }

    #[test]
    fn an_empty_json_report_is_an_empty_array() {
        let parsed: serde_json::Value =
            serde_json::from_str(&render(&BTreeMap::new(), OutsideWorkdir::Ask, true)).unwrap();
        assert_eq!(parsed, serde_json::json!([]));
    }
}
