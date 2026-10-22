//! `lev run continue <run> <message>` and `lev run fork <run> <message>`:
//! take a finished run up again with a new message, in place or as a new run.
//!
//! The run goes back to the stage its blueprint names in `continue_at` (the
//! entry stage by default) and works again as a new segment, with what each
//! of its regions keeps by its `on_continue`. A fork starts a new run
//! carrying that context instead, and leaves the finished run as it is.

use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};

/// Arguments for `lev run continue` and `lev run fork`.
#[derive(clap::Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct ContinueArgs {
    /// The finished run: complete, failed or cancelled.
    #[arg(value_name = "RUN")]
    pub run_id: String,
    /// The message the new segment starts with. A `@path` inside it attaches
    /// that file.
    #[arg(value_name = "MESSAGE")]
    pub message: String,
    /// The region the message lands in. The conversation when left out.
    #[arg(long)]
    pub region: Option<String>,
    /// Attach a file to the message: `path[:region][:type][:text]`, as on
    /// `lev run --attach`. Repeatable.
    #[arg(long, value_name = "PATH[:REGION][:TYPE][:text]")]
    pub attach: Vec<String>,
    /// Whether this command may start or replace the daemon it goes to.
    #[command(flatten)]
    pub daemon: crate::daemon::mode::DaemonModeArg,
}

/// Continue the run `args` names with its message (a new run forked from it,
/// with `fork`), and say which run is going again and in which segment.
pub async fn execute(
    client: &ControlClient,
    args: &ContinueArgs,
    fork: bool,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let (text, parts) = crate::commands::ctl::message_parts(&args.message, &args.attach, &cwd)?;
    let request = ControlRequest::Continue {
        run_id: args.run_id.clone(),
        text,
        region: args.region.clone(),
        parts,
        fork,
    };
    match client.request(&request).await? {
        ControlResponse::Continued {
            run_id,
            segment,
            forked_from: Some(source),
        } => println!("forked run {source} into run {run_id} (segment {segment})"),
        ControlResponse::Continued {
            run_id, segment, ..
        } => println!("run {run_id} continued: segment {segment}"),
        ControlResponse::Rejected { issues } => {
            anyhow::bail!("{}", leviath_runtime::continuation::refusal_text(&issues))
        }
        ControlResponse::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("the daemon answered something else: {other:?}"),
    }
    Ok(())
}

#[cfg(test)]
#[path = "continuing_tests.rs"]
mod tests;
