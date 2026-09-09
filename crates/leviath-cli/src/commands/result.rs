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

use std::path::PathBuf;

use clap::Args;

pub(crate) mod export;

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

    /// Write one produced file's bytes to stdout, by the name the run gave it
    /// (or into `--out`, when both are given).
    #[arg(long, value_name = "NAME", conflicts_with_all = ["json", "raw"])]
    pub artifact: Option<String>,

    /// Write the produced files into this directory: every one, or only the
    /// `--artifact` one. Prints each path written.
    #[arg(long, value_name = "DIR", conflicts_with_all = ["json", "raw"])]
    pub out: Option<PathBuf>,

    /// Hand one produced file to the operating system to open, by name.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["json", "raw", "artifact", "out"])]
    pub open: Option<String>,
}

/// Execute `lev result`.
pub(crate) async fn execute(args: ResultArgs) -> anyhow::Result<()> {
    let meta = crate::runstate::read_meta(&args.run_id)
        .map_err(|e| anyhow::anyhow!("no run '{}': {e}", args.run_id))?;
    // `meta.json` says whether there is an answer and how big; the bytes are in
    // the sidecar beside it.
    let output = crate::runstate::read_final_output(&args.run_id);
    // A missing answer is a failure exit rather than empty output, so
    // `lev result <id> > answer.txt` in a script does not silently write an
    // empty file and carry on.
    let no_answer = || {
        anyhow::anyhow!(
            "run '{}' produced no final output (status: {}). Only an agent that calls \
             `submit_output` has an answer to show; see `lev ps` for what it did.",
            args.run_id,
            meta.status
        )
    };
    let files = export::FileRequest::from_flags(
        args.artifact.as_deref(),
        args.out.as_deref(),
        args.open.as_deref(),
    );
    if let Some(request) = files {
        let output = output.as_ref().ok_or_else(no_answer)?;
        let mut stdout = std::io::stdout().lock();
        let lines = export::deliver(
            &args.run_id,
            &meta.workdir,
            output,
            request,
            leviath_sys::open_url,
            &mut stdout,
        )?;
        for line in lines {
            println!("{line}");
        }
        return Ok(());
    }
    match render(&args.run_id, output.as_ref(), args.json, args.raw) {
        Some(out) => {
            print!("{out}");
            Ok(())
        }
        None => Err(no_answer()),
    }
}

/// Render the answer, or `None` when the run never gave one. Pure, so the
/// formatting is directly testable.
fn render(
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
        for a in &output.artifacts {
            let sha = match a.sha256.is_empty() {
                true => String::new(),
                false => format!("  sha256:{}", a.sha256.chars().take(12).collect::<String>()),
            };
            out.push_str(&format!(
                "  {}  {}  {}  {}{sha}\n",
                a.name,
                a.path,
                a.media_type,
                leviath_core::media::human_size(a.size)
            ));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests;
