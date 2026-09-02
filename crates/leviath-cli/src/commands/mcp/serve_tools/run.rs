//! The `run` and `wait` tools: spawn a run, follow it, report how it ended.

use super::*;

/// The bundled agents `run` installs before resolving `agent`: the agent
/// itself when it is bundled and not installed, and `coder` alongside the
/// orchestrator, whose fan-out workers are coders.
pub(crate) fn missing_bundled(
    agent: &str,
    agents_dir: Option<&Path>,
) -> Vec<&'static crate::bundled::BundledAgent> {
    let Some(agents_dir) = agents_dir else {
        return Vec::new();
    };
    let mut wanted = vec![agent];
    if agent == "orchestrator" {
        wanted.push("coder");
    }
    crate::bundled::BUNDLED_AGENTS
        .iter()
        .filter(|b| wanted.contains(&b.name))
        .filter(|b| {
            !agents_dir
                .join(b.name)
                .join(leviath_core::files::MANIFEST_FILENAME)
                .exists()
        })
        .collect()
}

/// Whether the blueprint behind `spawn` declares `[read_paths]` the config
/// grants. A `yolo` run waives the taint gate, so such an agent can send
/// private data out unasked; the caller has to choose that knowingly.
pub(crate) fn has_granted_read_paths(spawn: &SpawnArgs) -> bool {
    let Ok(content) = std::fs::read_to_string(&spawn.blueprint_path) else {
        return false;
    };
    let Ok(blueprint) = leviath_core::manifest::parse_manifest(&content) else {
        return false;
    };
    let Ok(config) = crate::config::Config::load() else {
        return false;
    };
    matches!(
        crate::read_path_report::build(&blueprint, &config, Path::new(&spawn.workdir)),
        Some(Ok(report)) if report.granted() > 0
    )
}

/// Wait on a run while emitting progress, when the call carried a token.
async fn wait_with_heartbeat(
    shared: &Shared,
    stream: WorldEventStream,
    run_id: &str,
    timeout: Option<Duration>,
    progress: &Progress,
) -> WaitOutcome {
    let mut on_event = |event: &WorldEvent| match event {
        WorldEvent::StageTransition { to, iteration, .. } => {
            progress.emit(&format!(
                "run {run_id}: entered stage {to} (iteration {iteration})"
            ));
        }
        WorldEvent::Status { status, stage, .. } => {
            progress.emit(&format!("run {run_id}: {status} in {stage}"));
        }
        _ => {}
    };
    let wait = wait_for_run_with(
        &shared.control,
        stream,
        run_id,
        &shared.env.runs_dir,
        timeout,
        &mut on_event,
        &shared.timing.wait,
    );
    let mut wait = std::pin::pin!(wait);
    let mut heartbeat = tokio::time::interval(shared.timing.heartbeat);
    heartbeat.tick().await; // the first tick is immediate
    loop {
        tokio::select! {
            outcome = &mut wait => return outcome,
            _ = heartbeat.tick() => progress.emit(&format!("run {run_id}: still running")),
        }
    }
}

/// The result of a `run` or `wait` call from how the wait ended.
fn outcome_result(
    shared: &Shared,
    run_id: &str,
    outcome: WaitOutcome,
    preface: &str,
    warnings: Vec<String>,
) -> CallOutcome {
    match outcome {
        WaitOutcome::Finished {
            status,
            final_output,
            error,
            tools_installed,
        } => {
            let is_error = matches!(status, RunStatus::Error | RunStatus::Cancelled);
            let mut text = preface.to_string();
            match &final_output {
                Some(output) => text.push_str(&output.content),
                None => text.push_str(&format!(
                    "run {run_id} finished with status {} and no final output; see `result`/`status`",
                    status.wire()
                )),
            }
            if let Some(e) = &error {
                text.push_str(&format!("\nerror: {e}"));
            }
            if !tools_installed.is_empty() {
                text.push_str(&format!(
                    "\nInstalled global tools: {} (inspect with lev tools)",
                    tools_installed.join(", ")
                ));
            }
            let stage = read_meta_from(&run_dir_in(&shared.env.runs_dir, run_id))
                .ok()
                .map(|m| m.current_stage);
            let structured = json!({
                "run_id": run_id,
                "status": status.wire(),
                "stage": stage,
                "final_output": final_output.as_ref().map(|o| json!({
                    "format": o.format,
                    "stage": o.stage,
                    "truncated": o.truncated,
                    "artifacts": o.artifacts,
                    "host_truncated": false,
                })),
                "error": error,
                "tools_installed": tools_installed,
                "warnings": warnings,
            });
            let location = output_location(shared, run_id);
            CallOutcome::Result(finish(text, is_error, structured, Some(&location)))
        }
        WaitOutcome::Interaction {
            run_id: asker,
            request,
        } => ok(
            format!(
                "{preface}run {asker} is waiting for input ({}): {}\ncall respond with \
                 request_id={} (run {asker}) then wait",
                request.stage_name, request.prompt, request.id
            ),
            json!({
                "run_id": run_id,
                "asking_run_id": asker,
                "status": "waiting_input",
                "request_id": request.id,
                "interaction": request,
                "warnings": warnings,
            }),
            None,
        ),
        WaitOutcome::TimedOut { status } => ok(
            format!(
                "{preface}run {run_id} is still going after the timeout; it was not cancelled: \
                 use `wait`, `status` or `cancel` with this run_id"
            ),
            json!({
                "run_id": run_id,
                "status": status.map(|s| s.wire()).unwrap_or("running"),
                "timed_out": true,
                "warnings": warnings,
            }),
            None,
        ),
        WaitOutcome::Lost { reason } => fail(
            format!(
                "{preface}lost track of run {run_id} ({reason}); it may still be going: use `status`"
            ),
            json!({ "run_id": run_id, "lost": true, "warnings": warnings }),
        ),
    }
}

pub(crate) async fn run(
    shared: &Shared,
    args: &Args,
    progress: &Progress,
    run_slot: &Arc<Mutex<Option<String>>>,
) -> CallOutcome {
    let task = str_arg(args, "task").unwrap_or_default();
    let agent = str_arg(args, "agent").unwrap_or_else(|| shared.args.default_agent.clone());
    let workdir_raw = str_arg(args, "workdir")
        .map(PathBuf::from)
        .or_else(|| shared.args.workdir.clone())
        .unwrap_or_else(|| PathBuf::from(&shared.env.default_cwd));
    if !workdir_raw.is_absolute() {
        return CallOutcome::InvalidParams(format!(
            "workdir '{}' must be an absolute path",
            workdir_raw.display()
        ));
    }
    let wait_for_it = bool_arg(args, "wait").unwrap_or(true);
    let timeout_secs = uint_arg(args, "timeout_secs").unwrap_or(0);
    let yolo_explicit = bool_arg(args, "yolo");
    let yolo = yolo_explicit.unwrap_or(!shared.args.attended);
    let model = str_arg(args, "model");
    let mut allow = shared.args.allow.clone();
    allow.extend(str_list_arg(args, "allow"));
    let max_depth = uint_arg(args, "max_depth").map(|d| d as usize);
    let no_seed_commands = bool_arg(args, "no_seed_commands").unwrap_or(false);
    let regions = str_map_arg(args, "regions");
    let output_format = str_arg(args, "output_format");
    let output_instructions = str_arg(args, "output_instructions");
    let output_schema = object_arg(args, "output_schema");
    let output_request =
        (output_format.is_some() || output_instructions.is_some() || output_schema.is_some())
            .then_some(OutputSpec {
                format: output_format,
                instructions: output_instructions,
                example: None,
                schema: output_schema,
                validator: None,
                on_validator_error: None,
            });

    let workdir = match std::fs::canonicalize(&workdir_raw) {
        Ok(dir) if dir.is_dir() => dir,
        _ => {
            return fail(
                format!(
                    "workdir '{}' is not a usable directory",
                    workdir_raw.display()
                ),
                json!({ "workdir": workdir_raw.display().to_string() }),
            );
        }
    };
    // `workdir` was just canonicalised, so the home it is compared against
    // has to be too, or a symlinked home is never refused. `main` resolves
    // the home once already; this covers an env built by another caller.
    let home = shared
        .env
        .home
        .as_deref()
        .map(crate::workdir_guard::canonical_home);
    if let WorkdirVerdict::Confirm(concern) =
        assess(&workdir, home.as_deref(), &shared.env.allowed_workdirs)
    {
        return fail(
            format!(
                "workdir {} is your home directory (or a filesystem root); pass an explicit \
                 project workdir, or add it to [security] allowed_workdirs. {} {}",
                workdir.display(),
                concern.headline(),
                concern.detail()
            ),
            json!({ "workdir": workdir.display().to_string(), "refused": "workdir" }),
        );
    }

    let mut notes: Vec<String> = Vec::new();
    for bundled in missing_bundled(&agent, shared.env.agents_dir.as_deref()) {
        // `missing_bundled` only answers with an agents dir present.
        let agents_dir = shared.env.agents_dir.as_deref().unwrap_or(Path::new(""));
        if let Err(e) = crate::bundled::install_bundled(bundled, agents_dir) {
            return fail(
                format!(
                    "bundled agent '{}' is not installed and could not be installed into {} \
                     ({e}); run `lev integrate <host>` or `lev add` first",
                    bundled.name,
                    agents_dir.display()
                ),
                json!({ "agent": agent }),
            );
        }
        tracing::info!(
            agent = bundled.name,
            "installed a bundled agent for a host run"
        );
        notes.push(format!(
            "installed bundled agent '{}' v{}",
            bundled.name, bundled.version
        ));
    }

    let workdir_text = workdir.to_string_lossy().to_string();
    let spawn_args = match resolve_spawn_args(LaunchRequest {
        path: &agent,
        task: Some(&task),
        stdin_is_terminal: &never_interactive,
        model,
        workdir: &workdir_text,
        yolo,
        allow,
        max_depth,
        regions,
        no_seed_commands,
        output_request,
    }) {
        Ok(spawn_args) => spawn_args,
        Err(e) => {
            return fail(
                format!("could not start agent '{agent}': {e}"),
                json!({ "agent": agent }),
            );
        }
    };
    if yolo_explicit.is_none() && has_granted_read_paths(&spawn_args) {
        return fail(
            format!(
                "agent '{agent}' declares [read_paths] your config grants; pass `yolo` \
                 explicitly: true waives the taint gate (private data may leave without a \
                 prompt), false stops to ask (answer with `respond`)"
            ),
            json!({ "agent": agent, "refused": "yolo" }),
        );
    }
    let mut warnings = held_checkpoint_warning_for_spawn(&spawn_args);
    warnings.extend(read_path_warning_for_spawn(&spawn_args));

    if let Err(e) = shared.daemon_ready().await {
        return fail(
            format!("the leviath daemon is not available: {e}"),
            json!({ "agent": agent }),
        );
    }
    // Subscribe before spawning so no event between the two is missed.
    let stream = match shared.control.subscribe().await {
        Ok(stream) => stream,
        Err(e) => {
            return fail(
                format!("the leviath daemon is not reachable ({e})"),
                json!({ "agent": agent }),
            );
        }
    };
    let spawned = match spawn_once(&shared.control, spawn_args).await {
        Ok(spawned) => spawned,
        Err(e) => return fail(e.to_string(), json!({ "agent": agent })),
    };
    let run_id = spawned.run_id;
    *run_slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(run_id.clone());
    progress.emit(&format!("run {run_id} started"));

    let mut preface = String::new();
    for line in notes.iter().chain(warnings.iter()) {
        preface.push_str(line);
        preface.push('\n');
    }
    if !wait_for_it {
        return ok(
            format!(
                "{preface}run {run_id} started; it continues in the background: use `wait`, \
                 `status` or `cancel` with this run_id"
            ),
            json!({
                "run_id": run_id,
                "status": "starting",
                "warnings": warnings,
                "installed_agents": notes,
            }),
            None,
        );
    }
    let timeout = (timeout_secs > 0).then(|| Duration::from_secs(timeout_secs));
    let outcome = wait_with_heartbeat(shared, stream, &run_id, timeout, progress).await;
    outcome_result(shared, &run_id, outcome, &preface, warnings)
}

pub(crate) async fn wait(
    shared: &Shared,
    args: &Args,
    progress: &Progress,
    run_slot: &Arc<Mutex<Option<String>>>,
) -> CallOutcome {
    let run_id = str_arg(args, "run_id").unwrap_or_default();
    let timeout_secs = uint_arg(args, "timeout_secs").unwrap_or(0);
    if let Err(e) = read_meta_from(&run_dir_in(&shared.env.runs_dir, &run_id)) {
        return fail(
            format!(
                "no run '{run_id}' on this machine ({e}); `list_runs` shows the ones there are"
            ),
            json!({ "run_id": run_id }),
        );
    }
    *run_slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(run_id.clone());
    if let Err(e) = shared.daemon_ready().await {
        return fail(
            format!("the leviath daemon is not available: {e}"),
            json!({ "run_id": run_id }),
        );
    }
    let stream = match shared.control.subscribe().await {
        Ok(stream) => stream,
        Err(e) => {
            return fail(
                format!("the leviath daemon is not reachable ({e})"),
                json!({ "run_id": run_id }),
            );
        }
    };
    let timeout = (timeout_secs > 0).then(|| Duration::from_secs(timeout_secs));
    let outcome = wait_with_heartbeat(shared, stream, &run_id, timeout, progress).await;
    outcome_result(shared, &run_id, outcome, "", Vec::new())
}
