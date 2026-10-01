//! The search half of `GET /api/runs`: which runs match `?q=`, and the
//! highlights that say where. Split out of `runs.rs` for size.

use std::ops::ControlFlow;

use leviath_runtime::state::context::RegionChange;
use leviath_runtime::state::{Change, RunEvent, StateDelta};

use super::super::run_file;
use super::*;

/// Phase one of search: keep the runs that could match, bounding how many of
/// them are allowed to cost a file read.
pub(crate) fn apply_search(runs: Vec<Arc<RunMeta>>, spec: &RunSpec) -> (Vec<Arc<RunMeta>>, bool) {
    let Some(ref q) = spec.q else {
        return (runs, false);
    };
    let budgeted = spec.searches_filesystem();
    let mut kept = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;
    for meta in runs {
        if budgeted {
            if scanned >= MAX_SEARCH_SCAN {
                truncated = true;
                break;
            }
            scanned += 1;
        }
        if matches_query(&meta, q, &spec.sources) {
            kept.push(meta);
        }
    }
    (kept, truncated)
}

/// Does this run match, according to the requested sources? Sources are OR-ed.
///
/// The cheap sources read already-parsed metadata. The deep ones read the
/// run's file and its stage logs, which is what [`MAX_SEARCH_SCAN`] bounds.
pub(crate) fn matches_query(meta: &RunMeta, q: &str, sources: &[Source]) -> bool {
    sources.iter().any(|source| match source {
        Source::Meta => meta_fields(meta)
            .iter()
            .any(|(_, text)| search::find_ignore_ascii_case(text, q).is_some()),
        Source::Files => meta
            .flags
            .modified_files
            .iter()
            .any(|path| search::find_ignore_ascii_case(path, q).is_some()),
        Source::Context => context_highlight(meta, q).is_some(),
        Source::Journal => journal_highlights(meta, q).is_some(),
        Source::Logs => stage_indices(&meta.run_id).iter().any(|idx| {
            let output = runstate::tail_stage_output(&meta.run_id, *idx, SEARCH_LOG_TAIL_BYTES);
            let operational = runstate::tail_stage_log(&meta.run_id, *idx, SEARCH_LOG_TAIL_BYTES);
            search::find_ignore_ascii_case(&output, q).is_some()
                || search::find_ignore_ascii_case(&operational, q).is_some()
        }),
    })
}

/// The stage indices a run recorded, from its ledger - the index of record,
/// rather than a `read_dir` of the directory its bytes happened to land in.
pub(crate) fn stage_indices(run_id: &str) -> Vec<usize> {
    runstate::read_stages_index(run_id)
        .iter()
        .map(|stage| stage.index)
        .collect()
}

/// The searchable `(name, text)` pairs already present in a `RunMeta`.
pub(crate) fn meta_fields(meta: &RunMeta) -> Vec<(String, String)> {
    let mut out = vec![
        ("run_id".to_string(), meta.run_id.clone()),
        ("agent_name".to_string(), meta.agent_name.clone()),
        ("agent_path".to_string(), meta.agent_path.clone()),
        ("task".to_string(), meta.task.clone()),
        ("workdir".to_string(), meta.workdir.clone()),
        ("current_stage".to_string(), meta.current_stage.clone()),
    ];
    if let Some(ref title) = meta.title {
        out.push(("title".to_string(), title.clone()));
    }
    if let Some(ref model) = meta.model {
        out.push(("model".to_string(), model.clone()));
    }
    if let Some(ref error) = meta.error {
        out.push(("error".to_string(), error.clone()));
    }
    // `callback_url` and `callback_secret` are deliberately absent. The secret
    // never leaves the process, and neither is something a user searches for.
    // Sorted so the highlight a search reports for a metadata match does not
    // depend on hash order.
    let mut entries: Vec<(&String, &String)> = meta.metadata.iter().collect();
    entries.sort();
    for (key, value) in entries {
        out.push((format!("metadata.{key}"), value.clone()));
    }
    out
}

/// Phase two: why this run matched, for the items actually being returned.
pub(crate) fn highlights_for(meta: &RunMeta, q: &str, sources: &[Source]) -> Vec<Highlight> {
    let mut out = Vec::new();
    for source in sources {
        if out.len() >= MAX_HIGHLIGHTS {
            break;
        }
        match source {
            Source::Meta => {
                for (field, text) in meta_fields(meta) {
                    if out.len() >= MAX_HIGHLIGHTS {
                        break;
                    }
                    if let Some(at) = search::find_ignore_ascii_case(&text, q) {
                        out.push(Highlight {
                            field,
                            snippet: search::snippet(&text, at),
                            stage: None,
                        });
                    }
                }
            }
            Source::Files => {
                if let Some(path) = meta
                    .flags
                    .modified_files
                    .iter()
                    .find(|p| search::find_ignore_ascii_case(p, q).is_some())
                {
                    out.push(Highlight {
                        field: "modified_files".to_string(),
                        snippet: path.clone(),
                        stage: None,
                    });
                }
            }
            Source::Context => out.extend(context_highlight(meta, q)),
            Source::Logs => out.extend(logs_highlights(meta, q)),
            Source::Journal => out.extend(journal_highlights(meta, q)),
        }
    }
    out.truncate(MAX_HIGHLIGHTS);
    out
}

/// Where in the run's context window the match is, named by region: the
/// window as of the run's last step.
pub(crate) fn context_highlight(meta: &RunMeta, q: &str) -> Option<Highlight> {
    let snapshot = runstate::read_context_snapshot(&meta.run_id)?;
    snapshot.regions.iter().find_map(|region| {
        region.entries.iter().find_map(|entry| {
            search::find_ignore_ascii_case(&entry.content, q).map(|at| Highlight {
                field: format!("context.{}", region.name),
                snippet: search::snippet(&entry.content, at),
                stage: None,
            })
        })
    })
}

/// Which stage's log the match is in - so a client can then fetch that stage.
///
/// One highlight per stage at most, and the two streams are tried in the order
/// a person reads them: the assistant's own output first, the operational log
/// second. Expressed as a `find_map` rather than a loop with early returns
/// because the caller already caps the total, so there is nothing here that
/// needs to bail out partway.
pub(crate) fn logs_highlights(meta: &RunMeta, q: &str) -> Vec<Highlight> {
    stage_indices(&meta.run_id)
        .into_iter()
        .filter_map(|idx| {
            let output = runstate::tail_stage_output(&meta.run_id, idx, SEARCH_LOG_TAIL_BYTES);
            if let Some(at) = search::find_ignore_ascii_case(&output, q) {
                return Some(Highlight {
                    field: "logs.output".to_string(),
                    snippet: search::snippet(&output, at),
                    stage: Some(idx),
                });
            }
            let operational = runstate::tail_stage_log(&meta.run_id, idx, SEARCH_LOG_TAIL_BYTES);
            search::find_ignore_ascii_case(&operational, q).map(|at| Highlight {
                field: "logs.operational".to_string(),
                snippet: search::snippet(&operational, at),
                stage: Some(idx),
            })
        })
        .take(MAX_HIGHLIGHTS)
        .collect()
}

/// Where in the run's history the match is: a tool call or its result, a
/// question and its answer, a message, or text that entered the context at
/// some step.
///
/// Reads the run file's steps, the typed record of everything that happened,
/// and never the webhook's signing secret: the spec that holds it is not a
/// step, so no snippet can be cut from it.
pub(crate) fn journal_highlights(meta: &RunMeta, q: &str) -> Option<Highlight> {
    let reader = run_file::open(&meta.run_id).ok()??;
    let stages = &reader.spec().graph.stages;
    let mut found = None;
    let walked = run_file::walk(&meta.run_id, &reader, &mut |step| {
        let stage = stages.iter().position(|s| s.name == step.cursor.stage);
        found = in_step(step.delta, stage, q);
        match found {
            Some(_) => ControlFlow::Break(()),
            None => ControlFlow::Continue(()),
        }
    });
    walked.ok().and(found)
}

/// The first match in one step: its events, then the text its context
/// changes brought in. `stage` is the position of the stage it happened in.
fn in_step(delta: &StateDelta, stage: Option<usize>, q: &str) -> Option<Highlight> {
    let hit = |field: String, text: &str, stage: Option<usize>| {
        search::find_ignore_ascii_case(text, q).map(|at| Highlight {
            field,
            snippet: search::snippet(text, at),
            stage,
        })
    };
    let in_event = |event: &RunEvent| match event {
        RunEvent::ToolStarted(call) => hit(
            format!("journal.tool.{}", call.name),
            &call.args.value().to_string(),
            stage,
        ),
        RunEvent::ToolFinished { result, .. } => {
            hit("journal.tool_result".to_string(), &result.text, stage)
        }
        // A question and the words somebody answered it with. Searchable
        // because "which run asked me about that" is a question people
        // actually have, and the prompt is where a tool's own arguments were
        // shown to them.
        RunEvent::Settled(settled) => hit(
            match &settled.tool {
                Some(name) => format!("journal.asked.{name}"),
                None => "journal.asked".to_string(),
            },
            &settled.prompt,
            None,
        ),
        RunEvent::Answered { answer, .. } => hit("journal.answered".to_string(), answer, None),
        RunEvent::Message(message) => hit("journal.message".to_string(), &message.text, None),
        _ => None,
    };
    let in_change = |change: &Change| match change {
        Change::Context(diff) => diff.regions.iter().find_map(|(name, _, entries)| {
            let (RegionChange::Append(entries) | RegionChange::Replace(entries)) =
                entries.as_ref()?;
            entries
                .iter()
                .find_map(|entry| hit(format!("journal.context.{name}"), &entry.text, None))
        }),
        _ => None,
    };
    delta
        .events
        .iter()
        .find_map(in_event)
        .or_else(|| delta.changes.iter().find_map(in_change))
}
