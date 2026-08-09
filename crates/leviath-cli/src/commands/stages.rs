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
}

/// Execute `lev stages`.
pub async fn execute(args: StagesArgs) -> anyhow::Result<()> {
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
        false => print_ledger(&stages, args.regions),
    }
    Ok(())
}

/// The table `lev stages` prints. Split out so its shape is assertable without
/// capturing stdout.
fn print_ledger(stages: &[StageRecord], with_regions: bool) {
    println!(
        "{:<20} {:<10} {:>10} {:>10} {:>10} {:>10}",
        "STAGE", "STATUS", "PROMPT", "OUTPUT", "CACHE RD", "CACHE WR"
    );
    for stage in stages {
        println!(
            "{:<20} {:<10} {:>10} {:>10} {:>10} {:>10}",
            truncate(&stage.name, 20),
            format!("{:?}", stage.status).to_lowercase(),
            stage.prompt_tokens,
            stage.completion_tokens,
            stage.cached_tokens,
            stage.cache_write_tokens,
        );
        if with_regions {
            // Largest first: the question this answers is which region is worth
            // its place, and that is decided at the top of the list.
            let mut regions: Vec<(&String, &usize)> = stage.region_tokens.iter().collect();
            regions.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
            for (name, tokens) in regions {
                println!("  {:<18} {:>42}", truncate(name, 18), tokens);
            }
        }
    }

    let prompt: usize = stages.iter().map(|s| s.prompt_tokens).sum();
    let output: usize = stages.iter().map(|s| s.completion_tokens).sum();
    let read: usize = stages.iter().map(|s| s.cached_tokens).sum();
    let written: usize = stages.iter().map(|s| s.cache_write_tokens).sum();
    println!(
        "{:<20} {:<10} {:>10} {:>10} {:>10} {:>10}",
        "TOTAL", "", prompt, output, read, written
    );
}

/// `s` cut to `width` **characters**.
///
/// Counted in characters rather than bytes because the width is a column count
/// in a table, and a byte cut would both misalign the column and land
/// mid-codepoint on any non-ASCII stage or region name.
fn truncate(s: &str, width: usize) -> String {
    match s.chars().count() > width {
        true => s.chars().take(width).collect(),
        false => s.to_string(),
    }
}

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
        print_ledger(&[record("ingest", 100)], false);
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
        print_ledger(&[r], true);
    }

    /// A long stage name is cut rather than breaking the columns, and cut on a
    /// char boundary - region and stage names are author-supplied text.
    #[test]
    fn a_long_name_is_truncated_on_a_char_boundary() {
        assert_eq!(truncate("short", 20), "short");
        assert_eq!(truncate(&"é".repeat(30), 5).chars().count(), 5);
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
        })
        .await
        .expect_err("a missing ledger is worth saying");
        assert!(err.to_string().contains("no stage ledger"), "{err}");
    }
}
