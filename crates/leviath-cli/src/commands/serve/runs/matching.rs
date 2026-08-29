//! The search half of `GET /api/runs`: which runs match `?q=`, and the
//! highlights that say where. Split out of `runs.rs` for size.

use super::*;

/// Phase one of search: keep the runs that could match, bounding how many of
/// them are allowed to cost a file read.
pub(super) fn apply_search(runs: Vec<RunMeta>, resolved: &Resolved) -> (Vec<RunMeta>, bool) {
    let Some(ref q) = resolved.q else {
        return (runs, false);
    };
    let budgeted = resolved.searches_filesystem();
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
        if matches_query(&meta, q, &resolved.sources) {
            kept.push(meta);
        }
    }
    (kept, truncated)
}

/// Does this run match, according to the requested sources? Sources are OR-ed.
///
/// Nothing here parses. The cheap sources read already-parsed metadata; the
/// deep ones substring-scan raw file bytes. Parsing is phase two's job, and it
/// only happens for the items actually being returned.
pub(super) fn matches_query(meta: &RunMeta, q: &str, sources: &[Source]) -> bool {
    sources.iter().any(|source| match source {
        Source::Meta => meta_fields(meta)
            .iter()
            .any(|(_, text)| search::find_ignore_ascii_case(text, q).is_some()),
        Source::Files => meta
            .flags
            .modified_files
            .iter()
            .any(|path| search::find_ignore_ascii_case(path, q).is_some()),
        Source::Context => scan_file(
            &runstate::run_dir(&meta.run_id).join(leviath_core::files::CONTEXT_FILE),
            q,
        ),
        Source::Journal => scan_file(
            &runstate::run_dir(&meta.run_id).join(leviath_core::files::ARCHIVE_FILE),
            q,
        ),
        Source::Logs => stage_indices(&meta.run_id).iter().any(|idx| {
            let output = runstate::tail_stage_output(&meta.run_id, *idx, SEARCH_LOG_TAIL_BYTES);
            let operational = runstate::tail_stage_log(&meta.run_id, *idx, SEARCH_LOG_TAIL_BYTES);
            search::find_ignore_ascii_case(&output, q).is_some()
                || search::find_ignore_ascii_case(&operational, q).is_some()
        }),
    })
}

/// The stage indices a run recorded, from `stages.json` - the index of record,
/// rather than a `read_dir` of the directory its bytes happened to land in.
pub(super) fn stage_indices(run_id: &str) -> Vec<usize> {
    runstate::read_stages_index(run_id)
        .iter()
        .map(|stage| stage.index)
        .collect()
}

/// Substring-scan a whole file's bytes without parsing it.
pub(super) fn scan_file(path: &std::path::Path, q: &str) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => search::contains_ignore_ascii_case(&bytes, q.as_bytes()).is_some(),
        Err(_) => false,
    }
}

/// The searchable `(name, text)` pairs already present in a `RunMeta`.
pub(super) fn meta_fields(meta: &RunMeta) -> Vec<(String, String)> {
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
pub(super) fn highlights_for(meta: &RunMeta, q: &str, sources: &[Source]) -> Vec<Highlight> {
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

/// Where in the run's context window the match is, named by region.
///
/// Parses `context.json` once. Never replays the journal: that deep-copies a
/// whole context window per recorded point, which is the cost this design
/// exists to avoid.
pub(super) fn context_highlight(meta: &RunMeta, q: &str) -> Option<Highlight> {
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
pub(super) fn logs_highlights(meta: &RunMeta, q: &str) -> Vec<Highlight> {
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

/// Where in the run's history the match is: a tool call, or the context as it
/// stood at some earlier point.
///
/// Both halves matter. Live-testing this against real journals turned up runs
/// that matched on `q_in=journal` and came back with **no highlight at all** -
/// a result with no explanation, which is precisely what search-on-the-server
/// was supposed to fix. The text was in the journal's context records, and only
/// tool batches were being looked at.
///
/// Reads entry *content* and tool calls, and deliberately never the `meta` field
/// of `Header`/`Progress`/`Checkpoint`. Those carry a whole `RunMeta` including
/// the webhook signing secret, and a snippet cut from those bytes would put it
/// in the response. That exclusion is structural - the code never reaches for
/// the field - rather than a filter applied afterwards.
///
/// One residual case is left, and documented rather than papered over: the phase
/// one filter scans the journal's raw bytes, which *do* include those repeated
/// metadata blocks. A query matching only there (a workdir path, say) yields a
/// run with no highlight. The same text is searchable, with a highlight, through
/// `q_in=meta`.
pub(super) fn journal_highlights(meta: &RunMeta, q: &str) -> Option<Highlight> {
    use leviath_core::run_archive::{RegionDelta, RunRecord};

    /// The first entry in a region whose content matches, named by region.
    fn in_entries(
        region_name: &str,
        entries: &[leviath_core::run_meta::RegionEntrySnapshot],
        q: &str,
    ) -> Option<Highlight> {
        entries.iter().find_map(|entry| {
            search::find_ignore_ascii_case(&entry.content, q).map(|at| Highlight {
                field: format!("journal.context.{region_name}"),
                snippet: search::snippet(&entry.content, at),
                stage: None,
            })
        })
    }

    /// The first match in one record, or `None` if it carries no matching text.
    fn in_record(record: &RunRecord, q: &str) -> Option<Highlight> {
        match record {
            RunRecord::ToolBatch {
                calls, stage_index, ..
            } => calls.iter().find_map(|call| {
                [&call.arguments, call.result.as_ref().unwrap_or(&call.name)]
                    .into_iter()
                    .find_map(|text| {
                        search::find_ignore_ascii_case(text, q).map(|at| Highlight {
                            field: format!("journal.tool.{}", call.name),
                            snippet: search::snippet(text, at),
                            stage: Some(*stage_index),
                        })
                    })
            }),
            RunRecord::ContextCheckpoint { snapshot, .. } => snapshot
                .regions
                .iter()
                .find_map(|region| in_entries(&region.name, &region.entries, q)),
            RunRecord::ContextDiff { delta, .. } | RunRecord::Progress { delta, .. } => {
                delta.regions.iter().find_map(|region| match region {
                    RegionDelta::Set(snapshot) => in_entries(&snapshot.name, &snapshot.entries, q),
                    RegionDelta::Append { name, entries, .. } => in_entries(name, entries, q),
                    // Carry no text of their own.
                    RegionDelta::Clear { .. } | RegionDelta::Remove { .. } => None,
                })
            }
            RunRecord::Checkpoint { context, .. } => context
                .regions
                .iter()
                .find_map(|region| in_entries(&region.name, &region.entries, q)),
            // Carry no searchable content of their own - only the metadata this
            // function must not cut a snippet from.
            RunRecord::Header { .. }
            | RunRecord::OwnershipChanged { .. }
            | RunRecord::StatusChanged { .. }
            | RunRecord::Inference { .. }
            | RunRecord::InferenceUsage { .. }
            | RunRecord::ToolCallDone { .. }
            | RunRecord::Message { .. } => None,
        }
    }

    // Streamed, stopping at the first matching record: parsing the whole
    // journal per returned item multiplied the history endpoint's biggest
    // allocation by the page size.
    let mut found = None;
    runstate::visit_run_records(&meta.run_id, &mut |record| match in_record(record, q) {
        Some(hit) => {
            found = Some(hit);
            std::ops::ControlFlow::Break(())
        }
        None => std::ops::ControlFlow::Continue(()),
    })?;
    found
}
