//! `lev run show <run>`: what a run's file holds, for a person or a program
//! to read.
//!
//! A run file holds the run's spec (everything it was resolved to: its graph,
//! its inputs, each stage's model and tools), the state it started in, and a
//! step for every change after. `show` prints the spec, the state after any
//! step (`--at`), or the steps themselves (`--deltas`), as TOML or JSON. It
//! reads the file and nothing else, so it works for a finished run and with
//! no daemon running.

use anyhow::{Context, bail};
use clap::Args;
use leviath_runtime::runfile::view;

use crate::runstate;

/// Arguments for `lev run show`.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct ShowArgs {
    /// The run's id, as `lev ps` lists it.
    #[arg(value_name = "RUN")]
    pub run_id: String,

    /// Show the run's state after step SEQ instead of its spec. Step 0 is
    /// the state it started in; `lev run show <run> --deltas ..` lists the
    /// steps there are.
    #[arg(long, value_name = "SEQ", conflicts_with = "deltas")]
    pub at: Option<u64>,

    /// Show the steps from FROM to TO, both included, instead of the spec:
    /// `3..7`, `3..` for every step from 3 on, `..7`, or `..` for them all.
    #[arg(long, value_name = "FROM..TO")]
    pub deltas: Option<String>,

    /// Print TOML (the default).
    #[arg(long, conflicts_with = "json")]
    pub toml: bool,

    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

/// Print what `args` asks for.
pub async fn execute(args: ShowArgs) -> anyhow::Result<()> {
    println!("{}", render(&args)?);
    Ok(())
}

/// What `args` asks for, as it is printed.
pub(crate) fn render(args: &ShowArgs) -> anyhow::Result<String> {
    let id = &args.run_id;
    let reader = runstate::run_file::open_in(&runstate::run_dir(id)).with_context(|| {
        format!(
            "run '{id}' has no run file to read; `lev ps --all` lists the runs there are, and \
             a run from an older release is converted when the daemon starts"
        )
    })?;
    let last = reader.last_seq();
    if let Some(range) = &args.deltas {
        let (from, to) = range_of(range, last)?;
        let deltas = reader.deltas(from, to)?;
        return Ok(match args.json {
            true => to_json(&deltas),
            false => view::deltas_toml(&deltas),
        });
    }
    if let Some(seq) = args.at {
        if seq > last {
            bail!("run '{id}' has no step {seq}: its last step is {last}");
        }
        let state = reader.state_at(seq)?;
        return Ok(match args.json {
            true => to_json(&state),
            false => view::state_toml(&state),
        });
    }
    Ok(match args.json {
        true => to_json(reader.spec()),
        false => view::spec_toml(reader.spec()),
    })
}

/// `value` as pretty JSON.
fn to_json<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string_pretty(value).expect("run file types are plain data")
}

/// The steps `FROM..TO` names, with an open end running to the first step
/// or the last one.
fn range_of(range: &str, last: u64) -> anyhow::Result<(u64, u64)> {
    let Some((from, to)) = range.split_once("..") else {
        bail!("--deltas '{range}' is not a range: write FROM..TO, such as 3..7, 3.. or ..7");
    };
    let bound = |text: &str, open: u64| -> anyhow::Result<u64> {
        match text.trim() {
            "" => Ok(open),
            n => n
                .parse()
                .map_err(|_| anyhow::anyhow!("--deltas '{range}': '{n}' is not a step number")),
        }
    };
    let (from, to) = (bound(from, 1)?, bound(to, last)?);
    if from > to {
        bail!("--deltas '{range}' runs backwards: FROM must not be after TO");
    }
    Ok((from, to))
}

#[cfg(test)]
#[path = "show_tests.rs"]
mod tests;
