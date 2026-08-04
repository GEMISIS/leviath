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

use crate::approvals::SafeSource;
use crate::config::Config;

/// Arguments for `lev approvals`.
#[derive(Args)]
pub struct ApprovalsArgs {
    #[command(subcommand)]
    pub command: ApprovalsCommand,
}

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
fn render(keys: &std::collections::BTreeMap<String, SafeSource>, json: bool) -> String {
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
    out
}

/// The agent whose per-agent entries to include. No `--agent` reports only what
/// every agent gets, and no agent is named `""`, so the empty name matches
/// nothing in `[agent_safe_commands]`.
fn agent_name(args: &SafeArgs) -> &str {
    args.agent.as_deref().unwrap_or("")
}

/// Run `lev approvals`.
pub async fn execute(args: ApprovalsArgs) -> anyhow::Result<()> {
    let ApprovalsCommand::Safe(safe) = args.command;
    let config = Config::load()?;
    // The blueprint layer is deliberately absent: it depends on which manifest
    // is being run, and `lev validate <agent>` is where a blueprint's own
    // declarations are reported.
    let keys = config.safe_keys_for_agent(agent_name(&safe), None);
    print!("{}", render(&keys, safe.json));
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
            false,
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
        };
        assert_eq!(agent_name(&args(Some("coder"))), "coder");
        assert_eq!(agent_name(&args(None)), "");
    }

    /// An empty report says why it is empty rather than printing a bare header.
    #[test]
    fn an_empty_report_explains_itself() {
        let out = render(&BTreeMap::new(), false);
        assert!(out.contains("defaults` is off"), "{out}");
    }

    #[test]
    fn the_json_report_is_machine_readable() {
        let out = render(&keys(&[("shell:ls", SafeSource::Default)]), true);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed[0]["key"], "shell:ls");
        assert_eq!(parsed[0]["source"], "default");
    }

    #[test]
    fn an_empty_json_report_is_an_empty_array() {
        let parsed: serde_json::Value =
            serde_json::from_str(&render(&BTreeMap::new(), true)).unwrap();
        assert_eq!(parsed, serde_json::json!([]));
    }
}
