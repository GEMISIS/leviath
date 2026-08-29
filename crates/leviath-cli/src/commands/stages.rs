//! `lev stages <run-id>` - the per-stage ledger a staged agent's cost lives in.
//!
//! `stages.json` has carried per-stage token counts for a while and nothing on
//! the CLI read it: `lev context` renders the context history, `lev result`
//! prints the answer, and the only readers of the ledger were the dashboard,
//! telemetry, and `serve`. For a staged agent the per-stage split *is* the
//! diagnosis - it is how an output stage billing 252,848 prompt tokens to emit
//! three characters was found, and how a profile stage that had run away to
//! 1.1M was. Read-only; sources everything from disk.

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
}

/// Execute `lev stages`.
pub(crate) async fn execute(args: StagesArgs) -> anyhow::Result<()> {
    let stages = crate::runstate::read_stages_index(&args.run_id);
    if stages.is_empty() {
        anyhow::bail!(
            "no stage ledger for run '{}' (no readable stages.json)",
            args.run_id
        );
    }
    match args.json {
        true => println!(
            "{}",
            serde_json::to_string_pretty(&stages).expect("a stage ledger serializes")
        ),
        false => print_ledger(&stages, args.regions, args.visits),
    }
    Ok(())
}

/// One cost cell.
///
/// `?` for unknown, never `$0.0000`: a stage whose calls could not be priced
/// has not been shown to be free, and a zero in this column is a claim that it
/// was. A leading `~` says the figure was reconstructed from published rates
/// rather than read off the provider's own answer.
///
/// Four decimals throughout, so the column stays aligned and a cheap routing
/// call still resolves to a hundredth of a cent.
fn cost_cell(cost_usd: Option<f64>, is_exact: bool) -> String {
    match cost_usd {
        None => "?".to_string(),
        Some(usd) if is_exact => format!("${usd:.4}"),
        Some(usd) => format!("~${usd:.4}"),
    }
}

/// The table `lev stages` prints. Split out so its shape is assertable without
/// capturing stdout.
fn print_ledger(stages: &[StageRecord], with_regions: bool, with_visits: bool) {
    println!(
        "{:<20} {:<10} {:>10} {:>10} {:>10} {:>10} {:>11}",
        "STAGE", "STATUS", "PROMPT", "OUTPUT", "CACHE RD", "CACHE WR", "COST"
    );
    for stage in stages {
        println!(
            "{:<20} {:<10} {:>10} {:>10} {:>10} {:>10} {:>11}",
            leviath_core::truncate_chars(&stage.name, 20),
            format!("{:?}", stage.status).to_lowercase(),
            stage.prompt_tokens,
            stage.completion_tokens,
            stage.cached_tokens,
            stage.cache_write_tokens,
            cost_cell(stage.cost_usd, stage.cost_is_exact),
        );
        if with_visits {
            for (n, visit) in stage.visits.iter().enumerate() {
                println!(
                    "  {:<18} {:<10} {:>10} {:>10} {:>10} {:>10} {:>11}",
                    format!("visit {}", n + 1),
                    match visit.left_at {
                        Some(_) => "left",
                        None => "in it",
                    },
                    visit.prompt_tokens,
                    visit.completion_tokens,
                    visit.cached_tokens,
                    visit.cache_write_tokens,
                    cost_cell(visit.cost_usd, visit.cost_is_exact),
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
                    "  {:<18} {:>42}",
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
    // One unpriced stage makes the whole total unknown, the same way one
    // unpriced call makes a stage's. Summing the rest would print a figure that
    // looks like the bill and is not.
    let total_cost = stages
        .iter()
        .try_fold(0.0, |acc, s| Some(acc + s.cost_usd?));
    let total_exact = stages.iter().all(|s| s.cost_is_exact);
    println!(
        "{:<20} {:<10} {:>10} {:>10} {:>10} {:>10} {:>11}",
        "TOTAL",
        "",
        prompt,
        output,
        read,
        written,
        cost_cell(total_cost, total_exact),
    );
    // The title call is billed to the run and to no stage of it, so this column
    // can legitimately sum to less than `lev ps` reports for the same run.
    println!("\nStage costs exclude the run's title call, which belongs to no stage.");
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

    #[test]
    fn the_ledger_prints_without_regions() {
        print_ledger(&[record("ingest", 100)], false, false);
    }

    /// An unknown cost prints as `?`, and a reconstructed one wears the `~` that
    /// says it is arithmetic rather than an invoice. A zero here would read as
    /// "this stage was free", which is the one thing it has not been shown to be.
    #[test]
    fn a_cost_cell_says_unknown_and_says_reconstructed() {
        assert_eq!(cost_cell(None, false), "?");
        assert_eq!(cost_cell(None, true), "?");
        assert_eq!(cost_cell(Some(0.0421), true), "$0.0421");
        assert_eq!(cost_cell(Some(0.0421), false), "~$0.0421");
    }

    /// One unpriced stage takes the TOTAL with it: the priced remainder is not
    /// the bill, and printing it as though it were is how a partial total gets
    /// quoted onward.
    #[test]
    fn the_visit_breakdown_and_an_unknown_total_print() {
        let mut priced = record("gather", 100);
        priced.begin_visit(10);
        priced.record_call(
            &leviath_core::run_meta::StageCall {
                prompt_tokens: 40,
                completion_tokens: 5,
                cost_usd: Some(0.002),
                cost_reported: true,
                ..Default::default()
            },
            11,
        );
        priced.close_visit(20);
        priced.begin_visit(30);
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
            let dir = base.join("runs").join(run_id);
            std::fs::create_dir_all(&dir).expect("runs dir");
            let mut rec = record("ingest", 16_832);
            rec.region_tokens.insert("data_preview".to_string(), 6692);
            let json = serde_json::to_string(&[rec]).expect("serializes");
            std::fs::write(dir.join("stages.json"), json).expect("write");
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
            })
            .await
            .expect("json is the same read, printed differently");
        })
        .await;
    }

    #[tokio::test]
    async fn a_run_with_no_ledger_is_an_error_rather_than_an_empty_table() {
        let err = execute(StagesArgs {
            run_id: "no-such-run".to_string(),
            json: false,
            regions: false,
            visits: false,
        })
        .await
        .expect_err("a missing ledger is worth saying");
        assert!(err.to_string().contains("no stage ledger"), "{err}");
    }
}
