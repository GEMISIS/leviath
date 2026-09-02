//! The read-only tools: `status`, `result`, `list_runs`, `list_agents`.

use super::*;

/// The daemon's live word for a run, in the wire vocabulary, when it has one.
async fn live_status(shared: &Shared, run_id: &str) -> Option<String> {
    shared.daemon_ready().await.ok()?;
    let request = ControlRequest::Status {
        run_id: run_id.to_string(),
    };
    match shared.control.request(&request).await {
        Ok(ControlResponse::Status {
            status: Some(status),
        }) => Some(wire_status(status.label())),
        _ => None,
    }
}

pub(crate) async fn status(shared: &Shared, args: &Args) -> CallOutcome {
    let run_id = str_arg(args, "run_id").unwrap_or_default();
    let meta = match read_meta_from(&run_dir_in(&shared.env.runs_dir, &run_id)) {
        Ok(meta) => meta,
        Err(e) => {
            return fail(
                format!("no run '{run_id}' on this machine ({e})"),
                json!({ "run_id": run_id }),
            );
        }
    };
    let live = match is_terminal_status(&meta.status) {
        true => None,
        false => live_status(shared, &run_id).await,
    };
    let status = live
        .clone()
        .unwrap_or_else(|| meta.status.wire().to_string());
    ok(
        format!(
            "run {run_id}: {status}, stage '{}' (iteration {}), {} prompt / {} completion tokens{}{}",
            meta.current_stage,
            meta.iteration,
            meta.prompt_tokens,
            meta.completion_tokens,
            match meta.final_output.is_some() {
                true => ", final output available via `result`",
                false => "",
            },
            meta.error
                .as_deref()
                .map(|e| format!("\nerror: {e}"))
                .unwrap_or_default()
        ),
        json!({
            "run_id": run_id,
            "status": status,
            "stage": meta.current_stage,
            "iteration": meta.iteration,
            "tokens": { "prompt": meta.prompt_tokens, "completion": meta.completion_tokens },
            "error": meta.error,
            "has_final_output": meta.final_output.is_some(),
            "live": live.is_some(),
        }),
        None,
    )
}

pub(crate) fn result(shared: &Shared, args: &Args) -> CallOutcome {
    let run_id = str_arg(args, "run_id").unwrap_or_default();
    let offset = uint_arg(args, "offset").unwrap_or(0) as usize;
    // Clamped to what one result can carry, so `bytes` and `next_offset`
    // describe the page the host actually receives: a page `finish` had to
    // cut would report bytes the host never saw.
    let max_bytes = uint_arg(args, "max_bytes")
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX).min(MCP_TEXT_CAP))
        .unwrap_or(MCP_TEXT_CAP);
    let dir = run_dir_in(&shared.env.runs_dir, &run_id);
    let meta = match read_meta_from(&dir) {
        Ok(meta) => meta,
        Err(e) => {
            return fail(
                format!("no run '{run_id}' on this machine ({e})"),
                json!({ "run_id": run_id }),
            );
        }
    };
    let Some(output) = read_final_output_in(&dir, &meta) else {
        return fail(
            format!(
                "run {run_id} has no final output (status: {}); only an agent that calls \
                 `submit_output` has an answer to show",
                meta.status.wire()
            ),
            json!({ "run_id": run_id, "status": meta.status.wire() }),
        );
    };
    let total = output.content.len();
    let start = floor_char_boundary(&output.content, offset);
    let page = substring(&output.content, start, start.saturating_add(max_bytes));
    let end = start + page.len();
    let location = output_location(shared, &run_id);
    ok(
        page.to_string(),
        json!({
            "run_id": run_id,
            "status": meta.status.wire(),
            "offset": start,
            "bytes": page.len(),
            "total_bytes": total,
            "next_offset": (end < total).then_some(end),
            "final_output": {
                "format": output.format,
                "stage": output.stage,
                "submitted_at": output.submitted_at,
                "truncated": output.truncated,
                "artifacts": output.artifacts,
                "host_truncated": false,
            },
        }),
        Some(&location),
    )
}

pub(crate) async fn list_runs(shared: &Shared, args: &Args) -> CallOutcome {
    let limit = uint_arg(args, "limit").unwrap_or(20) as usize;
    let include_disk = bool_arg(args, "include_finished_on_disk").unwrap_or(true);
    let listing = match shared.daemon_ready().await {
        Ok(()) => shared.control.request(&ControlRequest::List).await.ok(),
        Err(_) => None,
    };
    let (mut rows, daemon_reachable) = match listing {
        Some(ControlResponse::List { runs, finished, .. }) => (
            runs.iter()
                .chain(finished.iter())
                .map(|entry| {
                    json!({
                        "run_id": entry.run_id,
                        "status": wire_status(entry.status.label()),
                        "stage": entry.stage,
                        "title": entry.title,
                        "started_at": entry.started_at,
                        "has_final_output": entry.has_final_output,
                        "live": true,
                    })
                })
                .collect::<Vec<Value>>(),
            true,
        ),
        _ => (Vec::new(), false),
    };
    if include_disk {
        let known: HashSet<String> = rows
            .iter()
            .filter_map(|r| r["run_id"].as_str().map(str::to_string))
            .collect();
        for meta in list_runs_in(&shared.env.runs_dir) {
            if known.contains(&meta.run_id) {
                continue;
            }
            rows.push(json!({
                "run_id": meta.run_id,
                "status": meta.status.wire(),
                "stage": meta.current_stage,
                "title": meta.title,
                "started_at": meta.started_at,
                "has_final_output": meta.final_output.is_some(),
                "live": false,
            }));
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r["started_at"].as_i64().unwrap_or(0)));
    rows.truncate(limit);
    let mut lines: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "{}  {}  stage '{}'{}",
                r["run_id"].as_str().unwrap_or(""),
                r["status"].as_str().unwrap_or(""),
                r["stage"].as_str().unwrap_or(""),
                r["title"]
                    .as_str()
                    .map(|t| format!("  {t}"))
                    .unwrap_or_default()
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push("no runs".to_string());
    }
    if !daemon_reachable {
        lines.push("(the daemon did not answer; only runs on disk are listed)".to_string());
    }
    ok(
        lines.join("\n"),
        json!({ "daemon_reachable": daemon_reachable, "runs": rows }),
        None,
    )
}

pub(crate) fn list_agents(shared: &Shared) -> CallOutcome {
    let mut agents: Vec<Value> = Vec::new();
    let mut installed: HashSet<String> = HashSet::new();
    let entries = shared
        .env
        .agents_dir
        .as_deref()
        .and_then(|dir| std::fs::read_dir(dir).ok());
    if let Some(entries) = entries {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let manifest = entry.path().join(leviath_core::files::MANIFEST_FILENAME);
            let Ok(content) = std::fs::read_to_string(&manifest) else {
                continue; // not an agent directory
            };
            let bundled = crate::bundled::BUNDLED_AGENTS
                .iter()
                .any(|b| b.name == name);
            agents.push(match leviath_core::manifest::parse_manifest(&content) {
                Ok(blueprint) => json!({
                    "name": name,
                    "agent_name": blueprint.name,
                    "version": blueprint.version,
                    "description": blueprint.description,
                    "accepts_task": blueprint.accepts_task(),
                    "caller_inputs": blueprint.caller_inputs(),
                    "bundled": bundled,
                    "installed": true,
                    "path": manifest.display().to_string(),
                }),
                Err(e) => json!({
                    "name": name,
                    "bundled": bundled,
                    "installed": true,
                    "path": manifest.display().to_string(),
                    "error": format!("does not parse: {e}"),
                }),
            });
            installed.insert(name);
        }
    }
    for bundled in crate::bundled::BUNDLED_AGENTS {
        if installed.contains(bundled.name) {
            continue;
        }
        let description = bundled
            .files
            .iter()
            .find(|(rel, _)| *rel == leviath_core::files::MANIFEST_FILENAME)
            .and_then(|(_, content)| leviath_core::manifest::parse_manifest(content).ok())
            .map(|b| b.description)
            .unwrap_or_default();
        agents.push(json!({
            "name": bundled.name,
            "version": bundled.version,
            "description": description,
            "bundled": true,
            "installed": false,
            "note": "bundled with Leviath; `run` installs it on demand",
        }));
    }
    agents.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let cwd_has_manifest = Path::new(&shared.env.default_cwd)
        .join(leviath_core::files::MANIFEST_FILENAME)
        .exists();
    let mut lines: Vec<String> = agents
        .iter()
        .map(|a| {
            let name = a["name"].as_str().unwrap_or("");
            let version = a["version"].as_str().unwrap_or("?");
            let tag = match (a["installed"].as_bool(), a["bundled"].as_bool()) {
                (Some(false), _) => " (bundled, installed on demand)",
                (_, Some(true)) => " (bundled)",
                _ => "",
            };
            match a.get("error").and_then(Value::as_str) {
                Some(err) => format!("{name} v{version}{tag}: {err}"),
                None => format!(
                    "{name} v{version}{tag}: {}",
                    a["description"].as_str().unwrap_or("")
                ),
            }
        })
        .collect();
    if cwd_has_manifest {
        lines.push(format!(
            "the working directory {} holds an agent.leviath; `run` with agent \".\" uses it",
            shared.env.default_cwd
        ));
    }
    ok(
        lines.join("\n"),
        json!({
            "agents": agents,
            "default_agent": shared.args.default_agent,
            "cwd_has_manifest": cwd_has_manifest,
        }),
        None,
    )
}
