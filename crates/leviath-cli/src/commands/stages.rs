//! `lev stages <run-id>` - the per-stage ledger a staged agent's cost lives in.
//!
//! A run's file carries per-stage token counts and spend. For a staged agent
//! the per-stage split *is* the diagnosis - it is how an output stage billing
//! 252,848 prompt tokens to emit three characters was found, and how a
//! profile stage that had run away to 1.1M was. Read-only; sources everything
//! from disk.

use anyhow::Context as _;
use clap::Args;
use leviath_core::run_meta::StageRecord;

/// Arguments for `lev stages`.
#[derive(Args, Debug)]
pub struct StagesArgs {
    /// The run id whose stage ledger to show.
    pub run_id: String,
    /// Print the ledger as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
    /// Include each stage's per-region token high-water marks.
    #[arg(long)]
    pub regions: bool,
    /// Break each stage down by visit, so a stage the run entered twice is two
    /// rows rather than one sum.
    #[arg(long)]
    pub visits: bool,
    /// List every model call the run sent, with how it ended, what it cost
    /// and how that cost is known.
    #[arg(long)]
    pub calls: bool,
}

/// Execute `lev stages`.
pub(crate) async fn execute(args: StagesArgs) -> anyhow::Result<()> {
    let stages = crate::runstate::read_stages_index(&args.run_id);
    if stages.is_empty() {
        anyhow::bail!(
            "no stage ledger for run '{}' (no readable run file)",
            args.run_id
        );
    }
    match args.calls {
        false => {
            show(&args, &stages, None);
            Ok(())
        }
        true => read_calls(&crate::runstate::run_dir(&args.run_id))
            .map(|calls| show(&args, &stages, Some(&calls)))
            .context(format!("the calls of run '{}' do not read", args.run_id)),
    }
}

/// Print the ledger, and the run's calls when they were asked for.
fn show(args: &StagesArgs, stages: &[StageRecord], calls: Option<&[CallLine]>) {
    match (args.json, calls) {
        (true, None) => println!(
            "{}",
            serde_json::to_string_pretty(&stages).expect("a stage ledger serializes")
        ),
        (true, Some(calls)) => println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "stages": stages, "calls": calls }))
                .expect("a stage ledger and its calls serialize")
        ),
        (false, calls) => {
            print_ledger(stages, args.regions, args.visits);
            // The run's own figure, which the stages do not add up to: the
            // title call belongs to no stage.
            crate::runstate::read_meta(&args.run_id)
                .iter()
                .for_each(|meta| println!("{}", run_cost_lines(meta)));
            print_calls(calls.unwrap_or_default());
        }
    }
}

/// What the run as a whole cost, and whose calls that covers.
fn run_cost_lines(meta: &leviath_core::run_meta::RunMeta) -> String {
    let spent = crate::render::cost::Spent::of_run(meta);
    format!(
        "\nRun cost: {}\nThat is this run's own calls. A run it started as a sub-agent or a \
         fan-out worker is a run of its own, with its own cost: `lev ps --tree` lists them.",
        crate::render::cost::sentence(&spent)
    )
}

/// One model call a run sent, as `lev stages --calls` lists it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct CallLine {
    /// `stage`, `routing`, `title` or `compaction`.
    pub kind: String,
    /// The stage it was made for; empty for the title call.
    pub stage: String,
    /// The iteration it was made in.
    pub iteration: u32,
    /// The model it was sent to, as `provider/model`.
    pub model: String,
    /// `answered`, `failed` or `cut_off`.
    pub ending: String,
    /// `reported`, `computed` or `unknown`.
    pub cost_class: String,
    /// What it cost, when that is known.
    pub cost_usd: Option<f64>,
    /// Why it is not, when it is not, in plain words.
    pub unknown: Option<String>,
    /// The provider's own id for the call, when it gave one.
    pub provider_call_id: Option<String>,
}

/// Every call in the run file under `dir`, oldest first.
fn read_calls(dir: &std::path::Path) -> anyhow::Result<Vec<CallLine>> {
    let reader = crate::runstate::run_file::open_in(dir)?;
    Ok(calls_of(&reader.deltas(1, reader.last_seq())?))
}

/// The calls `deltas` record.
fn calls_of(deltas: &[leviath_runtime::state::StateDelta]) -> Vec<CallLine> {
    use leviath_runtime::state::RunEvent;
    deltas
        .iter()
        .flat_map(|delta| &delta.events)
        .filter_map(|event| match event {
            RunEvent::Inference {
                model,
                spend,
                kind,
                stage,
                iteration,
                ending,
                provider_call_id,
                ..
            } => Some(CallLine {
                kind: kind.label().to_string(),
                stage: stage.as_ref().map(ToString::to_string).unwrap_or_default(),
                iteration: *iteration,
                model: model.to_string(),
                ending: ending.label().to_string(),
                cost_class: spend.class().label().to_string(),
                cost_usd: spend.cost_usd(),
                unknown: spend
                    .unknown
                    .reasons()
                    .first()
                    .map(|(reason, _)| reason.reason().to_string()),
                provider_call_id: provider_call_id.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// The table `lev stages --calls` adds: nothing for a run with no call to
/// list.
fn print_calls(calls: &[CallLine]) {
    if calls.is_empty() {
        return;
    }
    println!(
        "\n{:<10} {:<18} {:>4} {:<34} {:<9} {:<9} {:>11}",
        "KIND", "STAGE", "ITER", "MODEL", "ENDED", "COST IS", "COST"
    );
    for call in calls {
        println!(
            "{:<10} {:<18} {:>4} {:<34} {:<9} {:<9} {:>11}",
            call.kind,
            leviath_core::truncate_chars(&call.stage, 18),
            call.iteration,
            leviath_core::truncate_chars(&call.model, 34),
            call.ending,
            call.cost_class,
            crate::render::cost::cell(call.cost_usd),
        );
        if let Some(why) = &call.unknown {
            println!("  unknown: {why}");
        }
    }
}

/// A stage's status spelled as `lev stages --json` and the API spell it
/// (`waiting_input`), which also sets the width of the STATUS column: the
/// longest of them, so every row lines up.
fn status_label(status: &leviath_core::run_meta::StageRunStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The table `lev stages` prints. Split out so its shape is assertable without
/// capturing stdout.
fn print_ledger(stages: &[StageRecord], with_regions: bool, with_visits: bool) {
    println!(
        "{:<20} {:<13} {:>10} {:>10} {:>10} {:>10} {:>11} {:>6}",
        "STAGE", "STATUS", "PROMPT", "OUTPUT", "CACHE RD", "CACHE WR", "COST", "RESUB"
    );
    for stage in stages {
        println!(
            "{:<20} {:<13} {:>10} {:>10} {:>10} {:>10} {:>11} {:>6}",
            leviath_core::truncate_chars(&stage.name, 20),
            status_label(&stage.status),
            stage.prompt_tokens,
            stage.completion_tokens,
            stage.cached_tokens,
            stage.cache_write_tokens,
            crate::render::cost::cell(stage.cost_usd),
            stage.resubmissions,
        );
        if with_visits {
            for (n, visit) in stage.visits.iter().enumerate() {
                println!(
                    "  {:<18} {:<13} {:>10} {:>10} {:>10} {:>10} {:>11}",
                    format!("visit {}", n + 1),
                    match visit.left_at {
                        Some(_) => "left",
                        None => "in it",
                    },
                    visit.prompt_tokens,
                    visit.completion_tokens,
                    visit.cached_tokens,
                    visit.cache_write_tokens,
                    crate::render::cost::cell(visit.cost_usd),
                );
            }
            // Said rather than left to be inferred from a short list: the row
            // above is the whole stage, and a reader comparing it against
            // visits that stop early deserves to know why they do.
            if stage.visit_count > stage.visits.len() {
                println!(
                    "  {:<18} {} of {} visits recorded; the stage row above covers them all",
                    "",
                    stage.visits.len(),
                    stage.visit_count,
                );
            }
        }
        if with_regions {
            // Largest first: the question this answers is which region is worth
            // its place, and that is decided at the top of the list.
            let mut regions: Vec<(&String, &usize)> = stage.region_tokens.iter().collect();
            regions.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
            for (name, tokens) in regions {
                println!(
                    "  {:<18} {:>45}",
                    leviath_core::truncate_chars(name, 18),
                    tokens
                );
            }
        }
    }

    let prompt: usize = stages.iter().map(|s| s.prompt_tokens).sum();
    let output: usize = stages.iter().map(|s| s.completion_tokens).sum();
    let read: usize = stages.iter().map(|s| s.cached_tokens).sum();
    let written: usize = stages.iter().map(|s| s.cache_write_tokens).sum();
    // One stage with an unknown cost makes the whole total unknown, the same
    // way one such call makes a stage's. Summing the rest would print a figure
    // that looks like the bill and is not.
    let total_cost = stages
        .iter()
        .try_fold(0.0, |acc, s| Some(acc + s.cost_usd?));
    let resubmissions: usize = stages.iter().map(|s| s.resubmissions).sum();
    println!(
        "{:<20} {:<13} {:>10} {:>10} {:>10} {:>10} {:>11} {:>6}",
        "TOTAL",
        "",
        prompt,
        output,
        read,
        written,
        crate::render::cost::cell(total_cost),
        resubmissions,
    );
    // The title call is billed to the run and to no stage of it, so this column
    // can legitimately sum to less than the run's own cost below it.
    println!(
        "\nA `?` is a cost that is unknown, never one that is nothing. Stage costs exclude \
         the run's title call, which belongs to no stage."
    );
}

/// `s` cut to `width` **characters**.
///
#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::run_meta::{StageRecord, StageRunStatus};

    fn record(name: &str, prompt: usize) -> StageRecord {
        let mut r = StageRecord::new(name.to_string(), 0);
        r.prompt_tokens = prompt;
        r.completion_tokens = 10;
        r.cached_tokens = 5;
        r.cache_write_tokens = 7;
        r.status = StageRunStatus::Complete;
        r
    }

    /// Every status fits the 13-character STATUS column, spelled as `--json`
    /// spells it, so no row is pushed out of line (`waiting_input` was printed
    /// as `waitinginput` in a 10-character column).
    #[test]
    fn every_status_fits_its_column() {
        use StageRunStatus::*;
        for status in [
            Pending,
            Active,
            WaitingInput,
            Paused,
            Complete,
            Error,
            Cancelled,
            Skipped,
        ] {
            let label = status_label(&status);
            assert!(!label.is_empty() && label.len() <= 13, "{label}");
        }
        assert_eq!(status_label(&WaitingInput), "waiting_input");
    }

    #[test]
    fn the_ledger_prints_without_regions() {
        print_ledger(&[record("ingest", 100)], false, false);
    }

    /// One unpriced stage takes the TOTAL with it: the priced remainder is not
    /// the bill, and printing it as though it were is how a partial total gets
    /// quoted onward.
    #[test]
    fn the_visit_breakdown_and_an_unknown_total_print() {
        let mut priced = record("gather", 100);
        priced.begin_visit(10, leviath_core::execution::mint_visit_id());
        priced.record_call(
            &leviath_core::run_meta::StageCall {
                prompt_tokens: 40,
                completion_tokens: 5,
                ..Default::default()
            },
            11,
        );
        priced.close_visit(20);
        priced.begin_visit(30, leviath_core::execution::mint_visit_id());
        let mut unpriced = record("answer", 50);
        unpriced.record_call(&leviath_core::run_meta::StageCall::default(), 40);
        // The cap is past, so the stage row is the only complete figure and the
        // table has to say so rather than let a short list read as the whole run.
        unpriced.visit_count = 300;
        print_ledger(&[priced, unpriced], false, true);
    }

    #[test]
    fn the_ledger_prints_region_sizes_largest_first() {
        let mut r = record("compute", 100);
        r.region_tokens.insert("small".to_string(), 10);
        r.region_tokens.insert("data_preview".to_string(), 6692);
        // Two the same size, so the name tie-break is a real comparison rather
        // than a branch that can never run: without it the row order would
        // shuffle between runs of the same command.
        r.region_tokens.insert("alpha".to_string(), 42);
        r.region_tokens.insert("beta".to_string(), 42);
        print_ledger(&[r], true, false);
    }

    /// A run whose ledger is on disk, in an isolated runs dir.
    async fn with_ledger<R, Fut>(unique: &str, f: impl FnOnce(String) -> Fut) -> R
    where
        Fut: std::future::Future<Output = R>,
    {
        crate::runstate::with_isolated_runs_dir_async(unique, |base| async move {
            let run_id = "run-1";
            let _ = base;
            let mut rec = record("ingest", 16_832);
            rec.region_tokens.insert("data_preview".to_string(), 6692);
            crate::runstate::write_stages_index(run_id, &[rec]).expect("write");
            f(run_id.to_string()).await
        })
        .await
    }

    #[tokio::test]
    async fn the_table_reads_a_real_ledger() {
        with_ledger("stages-table", |run_id| async move {
            execute(StagesArgs {
                run_id,
                json: false,
                regions: true,
                visits: true,
                calls: false,
            })
            .await
            .expect("a ledger on disk is readable");
        })
        .await;
    }

    #[tokio::test]
    async fn the_json_form_reads_the_same_ledger() {
        with_ledger("stages-json", |run_id| async move {
            execute(StagesArgs {
                run_id,
                json: true,
                regions: false,
                visits: false,
                calls: false,
            })
            .await
            .expect("json is the same read, printed differently");
        })
        .await;
    }

    /// One call a run sent, as its file's step records it.
    fn inference(
        usd: f64,
        unknown: Option<leviath_core::cost::CostUnknown>,
    ) -> leviath_runtime::state::RunEvent {
        let mut reasons = leviath_core::cost::UnknownCalls::default();
        if let Some(reason) = unknown {
            reasons.add(reason);
        }
        leviath_runtime::state::RunEvent::Inference {
            attempt: "a1".into(),
            model: leviath_runtime::spec::names::ModelRef::parse("openrouter/openai/gpt-4.1-nano")
                .expect("a model"),
            spend: leviath_runtime::state::Spend {
                prompt_tokens: 192,
                completion_tokens: 1252,
                priced_usd: usd,
                reported_calls: u32::from(unknown.is_none()),
                unknown: reasons,
                ..Default::default()
            },
            finish_reason: None,
            kind: leviath_runtime::state::journal::CallKind::Stage,
            stage: Some(leviath_runtime::spec::names::StageName::new("work").expect("a name")),
            iteration: 1,
            ending: leviath_runtime::state::CallEnding::CutOff,
            provider_call_id: Some("gen-1".into()),
        }
    }

    /// `--calls` lists every call the run file records, with how it ended,
    /// its class and its cost, and says why a cost is unknown. Any other
    /// event is not a call.
    #[test]
    fn every_call_in_a_run_file_is_a_line() {
        use leviath_core::cost::CostUnknown;
        let delta = leviath_runtime::state::StateDelta {
            seq: 1,
            at: 2,
            changes: Vec::new(),
            events: vec![
                inference(0.00052, None),
                leviath_runtime::state::RunEvent::Log("not a call".into()),
                inference(0.0, Some(CostUnknown::NoLookup)),
            ],
        };
        let calls = calls_of(&[delta]);
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            CallLine {
                kind: "stage".into(),
                stage: "work".into(),
                iteration: 1,
                model: "openrouter/openai/gpt-4.1-nano".into(),
                ending: "cut_off".into(),
                cost_class: "reported".into(),
                cost_usd: Some(0.00052),
                unknown: None,
                provider_call_id: Some("gen-1".into()),
            }
        );
        assert_eq!(calls[1].cost_class, "unknown");
        assert_eq!(calls[1].cost_usd, None);
        assert_eq!(
            calls[1].unknown.as_deref(),
            Some(CostUnknown::NoLookup.reason())
        );
        print_calls(&calls);
        print_calls(&[]);
    }

    /// The calls are read off the run's own file: every step's, an error
    /// for a run with no file, and an error for a file with a step that
    /// does not decode, never a short list.
    #[tokio::test]
    async fn the_calls_are_read_off_the_run_file() {
        use crate::runstate::run_file::tests::{recorded, step_with};
        use leviath_runtime::runfile::codec::{FrameKind, encode};
        let runs = tempfile::tempdir().expect("a temp dir");
        let dir = recorded(runs.path());
        step_with(&dir, 5, vec![inference(0.25, None)], |_| {});
        let calls = read_calls(&dir).expect("the file reads");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].cost_usd, Some(0.25));

        assert!(read_calls(&runs.path().join("no-such-run")).is_err());

        let path = crate::runstate::run_file::path_in(&dir);
        let mut bytes = std::fs::read(&path).expect("the file is there");
        bytes.extend(encode(FrameKind::Delta, &1u64).expect("a frame encodes"));
        std::fs::write(&path, &bytes).expect("the file is written");
        assert!(read_calls(&dir).is_err());
    }

    #[tokio::test]
    async fn the_calls_print_under_the_table_and_join_the_json() {
        for json in [false, true] {
            with_ledger("stages-calls", |run_id| async move {
                execute(StagesArgs {
                    run_id,
                    json,
                    regions: false,
                    visits: false,
                    calls: true,
                })
                .await
                .expect("the calls of a readable run are listed");
            })
            .await;
        }
    }

    #[tokio::test]
    async fn a_run_with_no_ledger_is_an_error_rather_than_an_empty_table() {
        let err = execute(StagesArgs {
            run_id: "no-such-run".to_string(),
            json: false,
            regions: false,
            visits: false,
            calls: false,
        })
        .await
        .expect_err("a missing ledger is worth saying");
        assert!(err.to_string().contains("no stage ledger"), "{err}");
    }
}
