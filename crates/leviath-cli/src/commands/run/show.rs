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
#[command(
    after_help = "To run a blueprint named `show`, give an option before its name: \
                  lev run --task \"...\" show"
)]
pub struct ShowArgs {
    /// The run's id, as `lev ps` lists it, or the start of it when only one
    /// run's id starts that way.
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
    let id = &resolve(&args.run_id)?;
    let reader = runstate::run_file::open_in(&runstate::run_dir(id)).with_context(|| {
        format!(
            "run '{id}' has no run file this build reads; `lev ps --all` lists the runs there \
             are, and a run from an older release or an alpha build is converted when the \
             daemon starts"
        )
    })?;
    if let Some(note) = torn_note(id, reader.cut_bytes()) {
        eprintln!("{note}");
    }
    // On stderr, beside the torn-step note, so `--json` stays one document.
    for line in super::request::warnings_report(&reader.spec().warnings()) {
        eprintln!("{line}");
    }
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
        let state = leviath_runtime::runfile::as_it_stands(reader.state_at(seq)?);
        return Ok(match args.json {
            true => to_json(&state),
            false => view::state_toml(&state),
        });
    }
    let spec = reader.spec();
    Ok(match args.json {
        true => to_json(&SpecJson {
            warnings: spec.warnings().iter().map(ToString::to_string).collect(),
            spec,
        }),
        false => view::spec_toml(spec),
    })
}

/// The spec as JSON, with what may keep the run from ever finishing, one
/// line each, as `lev run --json` gives them. Left out when there is
/// nothing to say.
#[derive(serde::Serialize)]
struct SpecJson<'a> {
    #[serde(flatten)]
    spec: &'a leviath_runtime::spec::run_spec::RunSpec,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

/// The run `given` names: an exact id, or a prefix only one run's id starts
/// with. A prefix no run has is left as it is, so the read that follows says
/// there is no such run; one several share is refused with their ids.
fn resolve(given: &str) -> anyhow::Result<String> {
    if runstate::run_dir(given).is_dir() {
        return Ok(given.to_string());
    }
    let mut matches: Vec<String> = std::fs::read_dir(runstate::runs_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(given))
        .collect();
    matches.sort();
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => Ok(given.to_string()),
        many => bail!(
            "'{given}' is the start of {} runs' ids: {}; give more of the one you mean",
            many.len(),
            many.join(", ")
        ),
    }
}

/// What to say about a run file that ends in a step a crash left half
/// written, `cut` bytes of it: nothing when it ends whole.
pub(crate) fn torn_note(id: &str, cut: usize) -> Option<String> {
    (cut > 0).then(|| {
        format!(
            "warning: run '{id}' has a last step that was never finished being written \
             ({cut} bytes); it is left out here, and the daemon cuts it off when it next \
             opens the run"
        )
    })
}

/// What to add when `lev` could not parse `argv` (`refused`, as opposed to
/// printing help) and it began `lev run show`: that reads a run's file, so a
/// blueprint named `show` is run with the task first.
pub fn parse_hint(argv: &[String], refused: bool) -> Option<String> {
    let words: Vec<&str> = argv.iter().skip(1).take(2).map(String::as_str).collect();
    (refused && words == ["run", "show"]).then(|| {
        "note: `lev run show RUN` reads a run's file. To run a blueprint named show, give the \
         task first: lev run --task <TASK> show"
            .to_string()
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
