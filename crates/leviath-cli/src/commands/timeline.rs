//! `lev timeline <run-id>` - where a run's wall-clock time went.
//!
//! `lev stages` answers "what did each stage cost"; this answers "what was the
//! run doing for an hour". Everything is read from the journal (`run.lvr`),
//! which already timestamps every model call, tool batch, tool result and
//! status change, so the split between model time, tool time and time spent
//! waiting on children is exact rather than inferred.
//!
//! The one heuristic is the warning about repeated large replies. A reply cut
//! off by the output cap and retried leaves a signature no other behaviour
//! does: back-to-back replies in the same stage, each thousands of tokens,
//! each the same size to within a few percent. A run of that shape lost twenty
//! minutes to five identical 23,000-token replies before anyone noticed, so
//! the command names it rather than leaving it in a column of numbers.

use clap::Args;
use leviath_core::run_archive::{InferenceKind, RunRecord};
use leviath_core::run_meta::{RunMeta, RunStatus};
use serde::Serialize;

/// Arguments for `lev timeline`.
#[derive(Args, Debug)]
pub struct TimelineArgs {
    /// The run id whose timeline to show.
    pub run_id: String,
    /// Print the timeline as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
    /// List every model call, not just the per-stage summary.
    #[arg(long)]
    pub calls: bool,
    /// Include the run's children (and theirs), one line per run, plus how
    /// many calls per model were in flight at once across the tree.
    #[arg(long)]
    pub tree: bool,
}

/// A reply this large, repeated back-to-back at the same size, is treated as
/// a retry of a reply the output cap cut off. Smaller repeats are ordinary
/// (a short answer twice is just a short answer twice).
const LARGE_REPLY_TOKENS: usize = 8_000;

/// Two consecutive replies within this fraction of each other count as "the
/// same size". A cut-off reply lands at the cap every time, give or take the
/// tokenizer.
const SAME_SIZE_TOLERANCE: f64 = 0.10;

/// One model call, from the moment the run had nothing else in flight to the
/// moment the usage record landed. Includes any time the call queued for an
/// inference slot, which is deliberate: the run experienced that as latency.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct CallSpan {
    /// Stage name (empty for the title call).
    pub stage: String,
    /// Stage-local iteration.
    pub iteration: usize,
    /// `stage`, `routing`, `title` or `compaction`.
    pub kind: String,
    /// Model id as the journal recorded it.
    pub model: String,
    /// Unix seconds the call is taken to have started.
    pub started_at: i64,
    /// Unix seconds the usage record landed.
    pub ended_at: i64,
    /// Prompt tokens billed (uncached part).
    pub prompt_tokens: usize,
    /// Prompt tokens served from cache.
    pub cached_tokens: usize,
    /// Output tokens.
    pub completion_tokens: usize,
}

impl CallSpan {
    /// Wall seconds the call took.
    pub(crate) fn secs(&self) -> i64 {
        self.ended_at - self.started_at
    }
}

/// Per-stage roll-up of [`CallSpan`]s.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct StageSummary {
    /// Stage name.
    pub name: String,
    /// Seconds spent in model calls for this stage.
    pub secs: i64,
    /// Number of model calls.
    pub calls: usize,
    /// Output tokens across all calls.
    pub output_tokens: usize,
    /// The largest single reply, in output tokens.
    pub largest_reply: usize,
}

/// Where the wall clock went.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub(crate) struct Totals {
    /// `updated_at - started_at`.
    pub wall: i64,
    /// Seconds in model calls (queueing included).
    pub inference: i64,
    /// Seconds running tool batches.
    pub tools: i64,
    /// Seconds parked: waiting on children, or on a person to answer a
    /// prompt (an approval, an `ask_user_*` tool, an interaction point).
    pub waiting: i64,
    /// Whatever is left: scheduling, persistence, gaps between records.
    pub other: i64,
}

/// The whole picture for one run.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct RunTimeline {
    /// The run.
    pub run_id: String,
    /// Its agent.
    pub agent_name: String,
    /// Its status, lower-case.
    pub status: String,
    /// Depth in the run tree (0 = root).
    pub depth: usize,
    /// Direct children.
    pub children: Vec<String>,
    /// The split.
    pub totals: Totals,
    /// Per stage, in first-seen order.
    pub stages: Vec<StageSummary>,
    /// Every model call, in order.
    pub calls: Vec<CallSpan>,
    /// Anything that looks like wasted time.
    pub warnings: Vec<String>,
}

/// Execute `lev timeline`.
pub(crate) async fn execute(args: TimelineArgs) -> anyhow::Result<()> {
    let root = load(&args.run_id)?;
    let mut runs = vec![root];
    if args.tree {
        let mut queue: Vec<String> = runs[0].children.clone();
        while let Some(id) = queue.pop() {
            // A child whose journal is gone is a line we cannot draw; the rest
            // of the tree is still worth showing.
            if let Ok(child) = load(&id) {
                queue.extend(child.children.iter().cloned());
                runs.push(child);
            }
        }
        runs.sort_by_key(|r| (r.depth, r.calls.first().map_or(0, |c| c.started_at)));
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&runs).expect("a timeline serializes")
        );
        return Ok(());
    }
    print_run(&runs[0], args.calls);
    if args.tree {
        print_tree(&runs);
    }
    Ok(())
}

/// Read one run's metadata and journal and reduce them to a [`RunTimeline`].
fn load(run_id: &str) -> anyhow::Result<RunTimeline> {
    let meta = crate::runstate::read_meta(run_id)
        .map_err(|e| anyhow::anyhow!("no readable meta.json for run '{run_id}': {e}"))?;
    let records = crate::runstate::read_run_archive(run_id)
        .ok_or_else(|| anyhow::anyhow!("no readable journal (run.lvr) for run '{run_id}'"))?;
    Ok(analyze(&meta, &records))
}

/// Reduce a run's journal to its timeline. Pure, so the shape is testable
/// without a runs directory.
pub(crate) fn analyze(meta: &RunMeta, records: &[RunRecord]) -> RunTimeline {
    let mut calls = Vec::new();
    let mut totals = Totals {
        wall: (meta.updated_at - meta.started_at).max(0),
        ..Totals::default()
    };
    // `prev` is the last moment the run was known to be doing something else,
    // so the next usage record's call is taken to have started there.
    let mut prev = meta.started_at;
    let mut waiting_since: Option<i64> = None;
    for record in records {
        match record {
            RunRecord::StatusChanged { status, at } => {
                if matches!(status, RunStatus::WaitingInput) {
                    waiting_since = Some(*at);
                } else if let Some(since) = waiting_since.take() {
                    totals.waiting += (*at - since).max(0);
                    prev = *at;
                }
            }
            RunRecord::InferenceUsage {
                kind,
                stage,
                iteration,
                model,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                at,
                ..
            } => {
                // A usage record while parked is a child's doing, journaled
                // through the parent (the title call is the usual one); it is
                // not this run's time.
                if waiting_since.is_none() {
                    let span = CallSpan {
                        stage: stage.clone(),
                        iteration: *iteration,
                        kind: kind.label().to_string(),
                        model: model.clone(),
                        started_at: prev.min(*at),
                        ended_at: *at,
                        prompt_tokens: *prompt_tokens,
                        cached_tokens: *cached_tokens,
                        completion_tokens: *completion_tokens,
                    };
                    totals.inference += span.secs();
                    calls.push(span);
                }
                prev = *at;
            }
            RunRecord::ToolCallDone { at, .. } => {
                totals.tools += (*at - prev).max(0);
                prev = *at;
            }
            _ => {}
        }
    }
    totals.other = (totals.wall - totals.inference - totals.tools - totals.waiting).max(0);
    let stages = summarize_stages(&calls);
    let warnings = repeated_large_replies(&calls);
    RunTimeline {
        run_id: meta.run_id.clone(),
        agent_name: meta.agent_name.clone(),
        status: format!("{:?}", meta.status).to_lowercase(),
        depth: meta.depth,
        children: meta.children.clone(),
        totals,
        stages,
        calls,
        warnings,
    }
}

/// Roll calls up per stage, keeping first-seen order so the table reads in
/// the order the run happened.
fn summarize_stages(calls: &[CallSpan]) -> Vec<StageSummary> {
    let mut stages: Vec<StageSummary> = Vec::new();
    for call in calls {
        let name = match call.kind.as_str() {
            "stage" | "routing" => call.stage.clone(),
            other => format!("({other})"),
        };
        let entry = match stages.iter_mut().find(|s| s.name == name) {
            Some(entry) => entry,
            None => {
                stages.push(StageSummary {
                    name,
                    secs: 0,
                    calls: 0,
                    output_tokens: 0,
                    largest_reply: 0,
                });
                stages.last_mut().expect("just pushed")
            }
        };
        entry.secs += call.secs();
        entry.calls += 1;
        entry.output_tokens += call.completion_tokens;
        entry.largest_reply = entry.largest_reply.max(call.completion_tokens);
    }
    stages
}

/// One warning per stage that shows the cut-off-and-retried signature.
fn repeated_large_replies(calls: &[CallSpan]) -> Vec<String> {
    let stage_calls: Vec<&CallSpan> = calls
        .iter()
        .filter(|c| c.kind == InferenceKind::Stage.label())
        .collect();
    let mut warnings = Vec::new();
    let mut i = 0;
    while i < stage_calls.len() {
        let mut j = i;
        while j + 1 < stage_calls.len() && same_large_reply(stage_calls[j], stage_calls[j + 1]) {
            j += 1;
        }
        if j > i {
            let run = &stage_calls[i..=j];
            let secs: i64 = run.iter().map(|c| c.secs()).sum();
            let size = run.iter().map(|c| c.completion_tokens).max().unwrap_or(0);
            warnings.push(format!(
                "stage `{}`: {} back-to-back replies of about {} output tokens ({}s). A reply \
                 cut off by the stage's output cap and retried looks exactly like this; check \
                 the stage's max_output_tokens against the size of what it is asked to write.",
                run[0].stage,
                run.len(),
                size,
                secs
            ));
        }
        i = j + 1;
    }
    warnings
}

/// Two consecutive stage calls that look like the same reply twice.
fn same_large_reply(a: &CallSpan, b: &CallSpan) -> bool {
    let (x, y) = (a.completion_tokens, b.completion_tokens);
    a.stage == b.stage
        && x >= LARGE_REPLY_TOKENS
        && y >= LARGE_REPLY_TOKENS
        && (x.abs_diff(y) as f64) <= SAME_SIZE_TOLERANCE * (x.max(y) as f64)
}

/// The per-run report. Split from `execute` so its shape is assertable
/// without capturing stdout.
fn print_run(run: &RunTimeline, with_calls: bool) {
    let t = &run.totals;
    println!(
        "{} ({}, {}) wall {} = model calls {} + tools {} + waiting on children or prompts {} + other {}",
        run.run_id,
        run.agent_name,
        run.status,
        hms(t.wall),
        hms(t.inference),
        hms(t.tools),
        hms(t.waiting),
        hms(t.other),
    );
    println!();
    println!(
        "{:<20} {:>9} {:>6} {:>10} {:>10}",
        "STAGE", "MODEL TIME", "CALLS", "OUTPUT", "LARGEST"
    );
    for s in &run.stages {
        println!(
            "{:<20} {:>9} {:>6} {:>10} {:>10}",
            leviath_core::truncate_chars(&s.name, 20),
            hms(s.secs),
            s.calls,
            s.output_tokens,
            s.largest_reply
        );
    }
    if with_calls {
        println!();
        println!(
            "{:>8} {:>7} {:<8} {:<16} {:>4} {:<30} {:>8} {:>8} {:>7}",
            "AT", "TOOK", "KIND", "STAGE", "IT", "MODEL", "PROMPT", "CACHED", "OUT"
        );
        let t0 = run.calls.first().map_or(0, |c| c.started_at);
        for c in &run.calls {
            println!(
                "{:>8} {:>7} {:<8} {:<16} {:>4} {:<30} {:>8} {:>8} {:>7}",
                format!("+{}", hms(c.started_at - t0)),
                hms(c.secs()),
                c.kind,
                leviath_core::truncate_chars(&c.stage, 16),
                c.iteration,
                leviath_core::truncate_chars(&c.model, 30),
                c.prompt_tokens,
                c.cached_tokens,
                c.completion_tokens
            );
        }
    }
    for w in &run.warnings {
        println!();
        println!("warning: {w}");
    }
}

/// The tree table: one line per run, then the peak number of calls per model
/// that were in flight (or queued for a slot) at the same moment.
fn print_tree(runs: &[RunTimeline]) {
    println!();
    println!(
        "{:<44} {:>5} {:>9} {:>9} {:>9} {:>9}",
        "RUN", "DEPTH", "WALL", "MODEL", "WAITING", "OTHER"
    );
    for r in runs {
        println!(
            "{:<44} {:>5} {:>9} {:>9} {:>9} {:>9}",
            leviath_core::truncate_chars(&r.run_id, 44),
            r.depth,
            hms(r.totals.wall),
            hms(r.totals.inference),
            hms(r.totals.waiting),
            hms(r.totals.other + r.totals.tools)
        );
    }
    println!();
    println!("{:<40} {:>10}", "MODEL", "PEAK CALLS");
    for (model, peak) in peak_in_flight(runs) {
        println!(
            "{:<40} {:>10}",
            leviath_core::truncate_chars(&model, 40),
            peak
        );
    }
}

/// For each model, the most calls across the tree that overlapped in time.
/// Sorted highest first so the model that queued is at the top.
fn peak_in_flight(runs: &[RunTimeline]) -> Vec<(String, usize)> {
    let mut by_model: std::collections::BTreeMap<&str, Vec<(i64, i64)>> = Default::default();
    for c in runs.iter().flat_map(|r| r.calls.iter()) {
        by_model
            .entry(c.model.as_str())
            .or_default()
            .push((c.started_at, c.ended_at));
    }
    let mut peaks: Vec<(String, usize)> = by_model
        .into_iter()
        .map(|(model, spans)| {
            // Classic sweep: +1 at each start, -1 at each end; ends sort before
            // starts at the same second so a call that begins as another ends
            // is not counted as overlapping it.
            let mut points: Vec<(i64, i32)> =
                spans.iter().flat_map(|&(s, e)| [(s, 1), (e, -1)]).collect();
            points.sort();
            let (mut cur, mut peak) = (0i32, 0i32);
            for (_, d) in points {
                cur += d;
                peak = peak.max(cur);
            }
            (model.to_string(), peak as usize)
        })
        .collect();
    peaks.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    peaks
}

/// Seconds as `h:mm:ss` or `m:ss`, whichever is shortest and still readable.
fn hms(secs: i64) -> String {
    let s = secs.max(0);
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    match h {
        0 => format!("{m}:{sec:02}"),
        _ => format!("{h}:{m:02}:{sec:02}"),
    }
}

#[cfg(test)]
mod tests;
