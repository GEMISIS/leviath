//! `lev context <run-id>` - show a run's context-window history.
//!
//! Replays the run's portable archive (`run.lvr`) into the sequence of context
//! windows over time (one per recorded checkpoint/step) and prints them, so you
//! can inspect what the agent's memory looked like at each stage and point -
//! for debugging or auditing. Read-only; sources everything from disk.

use clap::Args;
use leviath_core::run_archive::RunPoint;

/// Arguments for `lev context`.
#[derive(Args, Debug)]
pub struct ContextArgs {
    /// The run id whose context-window history to show.
    pub run_id: String,
    /// Print the full history as JSON instead of a human-readable summary.
    #[arg(long)]
    pub json: bool,
    /// Include each region's entry contents (not just per-region summaries).
    #[arg(long)]
    pub full: bool,
}

/// Execute `lev context`.
pub(crate) async fn execute(args: ContextArgs) -> anyhow::Result<()> {
    let history = crate::runstate::context_history(&args.run_id);
    if history.is_empty() {
        anyhow::bail!(
            "no context history for run '{}' (no readable run.lvr archive)",
            args.run_id
        );
    }
    let out = render(&args.run_id, &history, args.json, args.full);
    print!("{out}");
    Ok(())
}

/// Render the history to a string (pure, so it's directly testable).
fn render(run_id: &str, history: &[RunPoint], json: bool, full: bool) -> String {
    if json {
        // RunPoint is Serialize; a plain array is the machine-readable form.
        return format!(
            "{}\n",
            serde_json::to_string_pretty(history).expect("RunPoint history always serializes")
        );
    }
    let mut out = String::new();
    out.push_str(&format!(
        "Context history for run '{run_id}' ({} point{}):\n\n",
        history.len(),
        if history.len() == 1 { "" } else { "s" }
    ));
    for (i, point) in history.iter().enumerate() {
        out.push_str(&format!(
            "[{}] {}  stage={}  iter={}  status={}  tokens={}/{}\n",
            i + 1,
            format_time(point.at),
            point.context.stage_name,
            point.meta.iteration,
            point.meta.status,
            point.context.total_tokens,
            point.context.max_tokens,
        ));
        for region in &point.context.regions {
            out.push_str(&format!(
                "      region {} ({}) - {} tok, {} entr{}\n",
                region.name,
                region.kind,
                region.current_tokens,
                region.entries.len(),
                if region.entries.len() == 1 {
                    "y"
                } else {
                    "ies"
                },
            ));
            if full {
                for entry in &region.entries {
                    for line in entry.content.lines() {
                        out.push_str(&format!("          {line}\n"));
                    }
                }
            }
        }
        out.push('\n');
    }
    out
}

/// Format a unix timestamp as a local `YYYY-MM-DD HH:MM:SS`, or the raw seconds
/// if it's out of range.
fn format_time(secs: i64) -> String {
    match chrono::DateTime::from_timestamp(secs, 0) {
        Some(dt) => dt
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => secs.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixtures;
    use leviath_core::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot, RunMeta};

    fn point(stage: &str, tokens: usize, entries: Vec<&str>) -> RunPoint {
        RunPoint {
            meta: RunMeta::new(
                "run-x".to_string(),
                "coder".to_string(),
                "/agents/coder".to_string(),
                "task".to_string(),
                None,
                "/work".to_string(),
                2,
            ),
            context: ContextSnapshot {
                stage_name: stage.to_string(),
                total_tokens: tokens,
                max_tokens: 1000,
                regions: vec![RegionSnapshot {
                    name: "conv".to_string(),
                    kind: "clearable".to_string(),
                    current_tokens: tokens,
                    max_tokens: 1000,
                    entries: entries
                        .into_iter()
                        .map(|c| RegionEntrySnapshot {
                            content: c.to_string(),
                            tokens: 1,
                            kind: leviath_core::region::EntryKind::Text,
                            metadata: None,
                            key: None,
                            taint: Default::default(),
                            reasoning: None,
                        })
                        .collect(),
                    description: None,
                }],
            },
            at: 0,
        }
    }

    #[test]
    fn render_human_lists_each_point_and_region() {
        let history = vec![
            point("plan", 1, vec!["hi"]),
            point("implement", 3, vec!["a", "b"]),
        ];
        let out = render("run-x", &history, false, false);
        assert!(out.contains("Context history for run 'run-x' (2 points)"));
        assert!(out.contains("[1]") && out.contains("stage=plan"));
        assert!(out.contains("[2]") && out.contains("stage=implement"));
        assert!(out.contains("region conv (clearable)"));
        // Summary mode doesn't dump entry contents.
        assert!(!out.contains("          hi"));
    }

    #[test]
    fn render_full_includes_entry_contents() {
        let history = vec![point("plan", 1, vec!["secret line"])];
        let out = render("run-x", &history, false, true);
        assert!(out.contains("secret line"));
        // Singular "point" / "entry" wording.
        assert!(out.contains("(1 point)"));
        assert!(out.contains("1 entry"));
    }

    #[test]
    fn render_json_is_a_parseable_array() {
        let history = vec![point("plan", 1, vec!["hi"])];
        let out = render("run-x", &history, true, false);
        let parsed: Vec<RunPoint> = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].context.stage_name, "plan");
    }

    #[test]
    fn format_time_handles_valid_and_out_of_range() {
        // A fixed timestamp (2023-11-14 UTC) formats as a date-time string.
        let formatted = format_time(1_700_000_000);
        assert!(formatted.contains('-') && formatted.contains(':'));
        // i64::MAX is out of DateTime range → falls back to the raw number.
        assert_eq!(format_time(i64::MAX), i64::MAX.to_string());
    }

    #[test]
    fn execute_prints_history_for_a_run_with_an_archive() {
        crate::runstate::with_isolated_runs_dir("context-execute-ok", |_d| {
            use leviath_core::run_archive::{self, RunIdentity, RunRecord};
            let run_id = "ctx-exec-run";
            std::fs::create_dir_all(crate::runstate::run_dir(run_id)).unwrap();
            let mut buf = Vec::new();
            run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION).unwrap();
            run_archive::write_record(
                &mut buf,
                &RunRecord::Header {
                    identity: RunIdentity {
                        run_id: run_id.to_string(),
                        machine_id: "m".to_string(),
                        world_id: "w".to_string(),
                        created_at: 0,
                    },
                    meta: Box::new(fixtures::run_meta(run_id)),
                },
            )
            .unwrap();
            run_archive::write_record(
                &mut buf,
                &RunRecord::ContextCheckpoint {
                    snapshot: ContextSnapshot {
                        stage_name: "plan".to_string(),
                        total_tokens: 1,
                        max_tokens: 100,
                        regions: vec![],
                    },
                    at: 1,
                },
            )
            .unwrap();
            std::fs::write(crate::runstate::run_dir(run_id).join("run.lvr"), &buf).unwrap();

            // Present archive → the success path (render + print) runs and returns Ok.
            let args = ContextArgs {
                run_id: run_id.to_string(),
                json: true,
                full: false,
            };
            let rt = tokio::runtime::Runtime::new().unwrap();
            assert!(rt.block_on(execute(args)).is_ok());
            // Missing archive → the error path.
            let missing = ContextArgs {
                run_id: "no-archive-run".to_string(),
                json: false,
                full: false,
            };
            assert!(rt.block_on(execute(missing)).is_err());
        });
    }
}
