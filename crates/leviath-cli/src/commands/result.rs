//! `lev result <run-id>` - print what an agent handed back.
//!
//! There was no way to read a finished run's answer from the command line. The
//! run's logs were on disk and `lev ps` reported its status, but the thing the
//! agent actually concluded lived nowhere a shell could reach it - the only
//! surface serving it was `GET /api/agents/{id}/result`, which needed a running
//! `lev serve`.
//!
//! Read-only and daemon-free: everything comes from the run's `meta.json`, so
//! this answers for a run that finished last week as readily as one that
//! finished a second ago.

use clap::Args;

/// Arguments for `lev result`.
#[derive(Args, Debug)]
pub struct ResultArgs {
    /// The run whose final output to print.
    pub run_id: String,

    /// Print the output and its metadata as JSON.
    #[arg(long)]
    pub json: bool,

    /// Print only the answer itself, with no heading and no trailing summary -
    /// what a shell pipeline wants.
    #[arg(long)]
    pub raw: bool,
}

/// Execute `lev result`.
pub(crate) async fn execute(args: ResultArgs) -> anyhow::Result<()> {
    let meta = crate::runstate::read_meta(&args.run_id)
        .map_err(|e| anyhow::anyhow!("no run '{}': {e}", args.run_id))?;
    // `meta.json` says whether there is an answer and how big; the bytes are in
    // the sidecar beside it.
    let output = crate::runstate::read_final_output(&args.run_id);
    match render(&args.run_id, output.as_ref(), args.json, args.raw) {
        Some(out) => {
            print!("{out}");
            Ok(())
        }
        // A missing answer is a failure exit rather than empty output, so
        // `lev result <id> > answer.txt` in a script does not silently write an
        // empty file and carry on.
        None => anyhow::bail!(
            "run '{}' produced no final output (status: {}). Only an agent that calls \
             `submit_output` has an answer to show; see `lev ps` for what it did.",
            args.run_id,
            meta.status
        ),
    }
}

/// Render the answer, or `None` when the run never gave one. Pure, so the
/// formatting is directly testable. `pub(crate)` because `lev run --wait`
/// prints a finished run's answer in exactly this shape.
pub(crate) fn render(
    run_id: &str,
    output: Option<&leviath_core::FinalOutput>,
    json: bool,
    raw: bool,
) -> Option<String> {
    let output = output?;
    if json {
        // The whole record, not just the content: a caller parsing this wants
        // the format label too, and whether the answer was cut short.
        return Some(format!(
            "{}\n",
            serde_json::to_string_pretty(output).expect("a final output always serializes")
        ));
    }
    if raw {
        // Content only. A trailing newline is added when the answer lacks one,
        // so the shell prompt does not end up glued to the last line.
        return Some(match output.content.ends_with('\n') {
            true => output.content.clone(),
            false => format!("{}\n", output.content),
        });
    }

    let mut out = String::new();
    let shape = output
        .format
        .as_deref()
        .map(|f| format!(" ({f})"))
        .unwrap_or_default();
    out.push_str(&format!(
        "Final output{shape} from run '{run_id}', stage '{}':\n\n",
        output.stage
    ));
    out.push_str(&output.content);
    if !output.content.ends_with('\n') {
        out.push('\n');
    }
    if output.truncated {
        out.push_str(
            "\n[truncated: the agent's answer exceeded the size limit and was cut short]\n",
        );
    }
    if !output.artifacts.is_empty() {
        out.push_str(&format!("\nFiles produced ({}):\n", output.artifacts.len()));
        for path in &output.artifacts {
            out.push_str(&format!("  {path}\n"));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests;
