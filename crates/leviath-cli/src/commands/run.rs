//! `lev run` - Run an agent

use clap::Args;
use leviath_core::blueprint::{
    EdgeTransform, ModelConfig, StageMode, StageResult, ToolResultRouting, TransitionCondition,
    TransitionEdge,
};
use leviath_core::layout::RegionDefinition;
use leviath_core::lifecycle::CompactionConfig;
use leviath_core::{Blueprint, ContextLayout, Region, RegionKind, Stage};
use leviath_runtime::{AgentEngine, AgentPool, AgentState, ContextWindow, ProviderRegistry};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_stream::StreamExt;

use tokio::sync::Mutex;

use crate::config::{Config, ToolPolicy};
use crate::runstate::{self, RunMeta, RunStatus, StageRecord, StageRunStatus};
use crate::tools::{resolve_policy, ToolRegistry};

// ─── Editor / task resolution ─────────────────────────────────────────────────

/// Resolve the task string from a CLI argument.
///
/// - `Some(s)` where `s` is an existing file path → read file contents.
/// - `Some(s)` otherwise → use `s` as a literal prompt.
/// - `None` when stdin is not a TTY → error.
/// - `None` when stdin is a TTY → launch the user's editor on a temp prompt file.
fn resolve_task(
    arg: &Option<String>,
    agent_name: &str,
    description: Option<&str>,
) -> anyhow::Result<String> {
    match arg {
        Some(s) => {
            let p = std::path::Path::new(s);
            if p.is_file() {
                let content = std::fs::read_to_string(p)
                    .map_err(|e| anyhow::anyhow!("Failed to read task file '{}': {}", s, e))?;
                let trimmed = content.trim().to_string();
                if trimmed.is_empty() {
                    anyhow::bail!("Task file '{}' is empty.", s);
                }
                return Ok(trimmed);
            }
            Ok(s.clone())
        }
        None => {
            use std::io::IsTerminal;
            if !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "No task provided. Pass --task \"<prompt>\" or --task <file>.\n\
                     (stdin is not a TTY, so the interactive editor cannot be used)"
                );
            }

            // Build a commented template file for the editor
            let mut template = format!("# Task for agent: {}\n", agent_name);
            if let Some(desc) = description {
                if !desc.is_empty() {
                    template.push_str(&format!("# {}\n", desc));
                }
            }
            template.push_str(
                "#\n# Describe your task below. Lines starting with '#' are ignored.\n\n",
            );

            // Write to a temp file
            let tmp_path =
                std::env::temp_dir().join(format!("lev-task-{}.txt", std::process::id()));
            std::fs::write(&tmp_path, &template)
                .map_err(|e| anyhow::anyhow!("Failed to create task temp file: {}", e))?;

            // Launch the editor (exits only when the user closes it)
            let result = launch_editor(&tmp_path);
            let content = std::fs::read_to_string(&tmp_path).unwrap_or_default();
            let _ = std::fs::remove_file(&tmp_path);
            result?;

            // Strip comment lines and trim
            let task: String = content
                .lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();

            if task.is_empty() {
                anyhow::bail!("Aborting run: empty task.");
            }
            Ok(task)
        }
    }
}

/// Launch the user's preferred editor on `path` and wait for it to exit.
///
/// Editor resolution order: $VISUAL → $EDITOR → platform default.
/// Platform defaults: Unix tries `vim` then `nano`; Windows uses `notepad`.
fn launch_editor(path: &std::path::Path) -> anyhow::Result<()> {
    use std::process::Command;

    // Resolve editor candidates in priority order
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(v) = std::env::var("VISUAL") {
        if !v.is_empty() {
            candidates.push(v);
        }
    }
    if let Ok(e) = std::env::var("EDITOR") {
        if !e.is_empty() {
            candidates.push(e);
        }
    }

    #[cfg(unix)]
    {
        candidates.push("vim".to_string());
        candidates.push("nano".to_string());
        candidates.push("vi".to_string());
    }
    #[cfg(windows)]
    {
        candidates.push("notepad".to_string());
    }
    // Final fallback
    if candidates.is_empty() {
        candidates.push("nano".to_string());
    }

    let path_str = path.to_string_lossy();

    for editor in &candidates {
        // Handle editor strings that may include flags (e.g. "code --wait")
        let parts: Vec<&str> = editor.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        let mut cmd = Command::new(parts[0]);
        for arg in &parts[1..] {
            cmd.arg(arg);
        }
        cmd.arg(path_str.as_ref());

        match cmd.status() {
            Ok(status) => {
                if status.success() || status.code().is_some() {
                    // Exited (even non-zero means the user closed it — treat as OK)
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Try next candidate
                continue;
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to launch editor '{}': {}",
                    editor,
                    e
                ));
            }
        }
    }

    anyhow::bail!("No editor found. Set $VISUAL or $EDITOR, or install vim/nano/notepad.")
}

// ─── Per-stage recorder ───────────────────────────────────────────────────────

/// Tracks the current stage index for tool-activity logging from the executor closure.
///
/// The executor closure is `move` and captures an `Arc<Mutex<usize>>` that is
/// updated by the stage loop before each stage runs. This lets the closure write
/// tool activity to the correct per-stage log without needing to restructure the
/// entire executor.
type CurrentStageIdx = Arc<Mutex<usize>>;

/// Write a line to the per-stage readable output (agent responses).
fn record_stage_output(run_id: &str, idx: usize, text: &str) {
    runstate::append_stage_output(run_id, idx, text);
}

/// Write a line to the per-stage operational/tool log.
fn record_stage_log(run_id: &str, idx: usize, text: &str) {
    runstate::append_stage_log(run_id, idx, text);
}

#[derive(Args)]
pub struct RunArgs {
    /// Path to agent project or agent.leviath (or installed agent name)
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Task prompt, or path to a file containing the task.
    /// Omit to open an interactive editor (requires a TTY).
    #[arg(short, long)]
    pub task: Option<String>,

    /// Model override
    #[arg(short, long)]
    pub model: Option<String>,

    /// Run in the foreground (inline, blocking) instead of the default background mode
    #[arg(short = 'f', long)]
    pub foreground: bool,

    /// Allow all tool calls without prompting for this run (same as --allow '*')
    #[arg(long)]
    pub yolo: bool,

    /// Allow specific tools without prompting (comma-separated, e.g. "read_file,bash")
    #[arg(long, value_delimiter = ',')]
    pub allow: Vec<String>,

    /// Require approval for specific tools even if they would be auto-allowed (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub ask: Vec<String>,

    /// Deny specific tools entirely for this run (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub deny: Vec<String>,

    /// Maximum sub-agent tree depth (overrides blueprint config, lower wins)
    #[arg(long)]
    pub max_depth: Option<usize>,

    /// Number of background instances to spawn (default: 1)
    #[arg(short = 'n', long, default_value = "1")]
    pub count: usize,
}

/// Arguments for the hidden `__run-worker` subcommand.
#[derive(Args)]
pub struct WorkerArgs {
    /// Path to agent manifest or directory
    #[arg(value_name = "PATH")]
    pub path: String,

    /// Task prompt
    #[arg(short, long)]
    pub task: String,

    /// Run ID for on-disk state tracking
    #[arg(long)]
    pub run_id: String,

    /// Model override
    #[arg(short, long)]
    pub model: Option<String>,

    /// Allow all tool calls without prompting
    #[arg(long, default_value_t = false)]
    pub yolo: bool,

    /// Tools to auto-allow (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub allow: Vec<String>,

    /// Tools to always ask about (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub ask: Vec<String>,

    /// Tools to deny entirely (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub deny: Vec<String>,

    /// Maximum sub-agent tree depth
    #[arg(long)]
    pub max_depth: Option<usize>,
}

pub async fn execute(args: RunArgs) -> anyhow::Result<()> {
    if args.foreground {
        if args.count > 1 {
            anyhow::bail!("--count is not supported with --foreground");
        }
        return run_foreground(args).await;
    }

    // Background mode: create run state, spawn detached worker process(es)
    let path = args.path.as_deref().unwrap_or(".").to_string();
    let manifest_path = find_manifest(&path)?;

    let manifest_content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("Failed to read manifest: {}", e))?;
    let blueprint = parse_manifest(&manifest_content)?;

    // Resolve the task once (may launch an interactive editor) before spawning workers.
    let description = Some(blueprint.description.as_str());
    let task = resolve_task(&args.task, &blueprint.name, description)?;

    let workdir = std::env::current_dir()?;
    let count = args.count.max(1);

    for i in 0..count {
        let run_id = runstate::new_run_id(&blueprint.name);

        let meta = RunMeta::new(
            run_id.clone(),
            blueprint.name.clone(),
            manifest_path
                .parent()
                .unwrap_or(Path::new("."))
                .to_string_lossy()
                .to_string(),
            task.clone(),
            args.model.clone(),
            workdir.to_string_lossy().to_string(),
            blueprint.stages.len(),
        );
        runstate::create_run(&meta)?;

        // Redirect the worker's stdout + stderr to the run's output.log
        let log_path = runstate::run_dir(&run_id).join("output.log");
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let log_file2 = log_file.try_clone()?;

        let exe = std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("Failed to locate current executable: {}", e))?;

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("__run-worker")
            .arg(manifest_path.to_string_lossy().as_ref())
            .arg("--task")
            .arg(&task)
            .arg("--run-id")
            .arg(&run_id);

        if let Some(ref model) = args.model {
            cmd.arg("--model").arg(model);
        }
        if args.yolo {
            cmd.arg("--yolo");
        }
        for t in &args.allow {
            cmd.arg("--allow").arg(t);
        }
        for t in &args.ask {
            cmd.arg("--ask").arg(t);
        }
        for t in &args.deny {
            cmd.arg("--deny").arg(t);
        }
        if let Some(md) = args.max_depth {
            cmd.arg("--max-depth").arg(md.to_string());
        }

        cmd.current_dir(&workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_file2));

        // On Unix: setsid() detaches the worker into its own session so it
        // survives the spawning terminal being closed.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        cmd.spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn worker process: {}", e))?;

        if count == 1 {
            println!("Started run: {}", run_id);
            println!("  lev dash  — monitor in TUI dashboard");
        } else {
            println!("  [{}/{}] Started run: {}", i + 1, count, run_id);
        }
    }

    if count > 1 {
        println!("Spawned {} runs. Use `lev dash` to monitor.", count);
    }

    Ok(())
}

// ─── Graph traversal runtime ────────────────────────────────────────────────

/// Determine whether a blueprint uses graph mode (any stage has transitions set).
fn is_graph_mode(blueprint: &Blueprint) -> bool {
    blueprint.stages.iter().any(|s| s.transitions.is_some())
}

/// Resolve the next transition from a stage, considering conditions, visit counts,
/// and LLM routing when multiple edges are available.
///
/// Returns `None` when the stage is terminal (no valid outgoing transitions).
#[allow(clippy::too_many_arguments)]
async fn resolve_transition(
    stage: &Stage,
    stage_idx: usize,
    blueprint: &Blueprint,
    visit_counts: &HashMap<String, usize>,
    stage_result: &StageResult,
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
) -> Option<(TransitionEdge, usize)> {
    match &stage.transitions {
        None => {
            // Linear mode: advance to next stage by index
            if stage_idx + 1 < blueprint.stages.len() {
                let next = &blueprint.stages[stage_idx + 1];
                let next_idx = stage_idx + 1;
                Some((
                    TransitionEdge {
                        target: next.name.clone(),
                        condition: TransitionCondition::Always,
                        hint: None,
                        transform: EdgeTransform::Direct,
                    },
                    next_idx,
                ))
            } else {
                None // terminal
            }
        }
        Some(transitions) => {
            if transitions.is_empty() {
                return None; // terminal stage
            }

            // Filter edges by visit count limits
            let available: Vec<(&String, &TransitionEdge)> = transitions
                .iter()
                .filter(|(target_name, _edge)| {
                    let target_stage = blueprint.find_stage(target_name);
                    if let Some(ts) = target_stage {
                        if let Some(max_rev) = ts.max_revisits {
                            let visits =
                                visit_counts.get(target_name.as_str()).copied().unwrap_or(0);
                            visits <= max_rev // allow first visit + max_revisits
                        } else {
                            true
                        }
                    } else {
                        false // unknown target, skip
                    }
                })
                .collect();

            if available.is_empty() {
                return None; // all targets exhausted
            }

            // Step 1: Error condition — auto-transition if error occurred
            if *stage_result == StageResult::Error {
                if let Some((_name, edge)) = available
                    .iter()
                    .find(|(_, e)| e.condition == TransitionCondition::Error)
                {
                    let target_idx = blueprint
                        .stages
                        .iter()
                        .position(|s| s.name == edge.target)
                        .unwrap_or(0);
                    return Some(((*edge).clone(), target_idx));
                }
            }

            // Step 2: MaxIterations condition — auto-transition
            if *stage_result == StageResult::MaxIterations {
                if let Some((_name, edge)) = available
                    .iter()
                    .find(|(_, e)| e.condition == TransitionCondition::MaxIterations)
                {
                    let target_idx = blueprint
                        .stages
                        .iter()
                        .position(|s| s.name == edge.target)
                        .unwrap_or(0);
                    return Some(((*edge).clone(), target_idx));
                }
            }

            // Step 3: Filter to only always/llm_choice edges for LLM prompt
            let choosable: Vec<(&String, &TransitionEdge)> = available
                .into_iter()
                .filter(|(_, e)| {
                    matches!(
                        e.condition,
                        TransitionCondition::Always | TransitionCondition::LlmChoice
                    )
                })
                .collect();

            match choosable.len() {
                0 => None, // terminal
                1 => {
                    let (_, edge) = choosable[0];
                    let target_idx = blueprint
                        .stages
                        .iter()
                        .position(|s| s.name == edge.target)
                        .unwrap_or(0);
                    Some((edge.clone(), target_idx))
                }
                _ => {
                    // Multiple edges: prompt LLM to choose
                    let chosen = prompt_llm_transition(
                        stage,
                        &choosable,
                        engine,
                        entity,
                        provider_name,
                        model_name,
                    )
                    .await;
                    match chosen {
                        Some(edge) => {
                            let target_idx = blueprint
                                .stages
                                .iter()
                                .position(|s| s.name == edge.target)
                                .unwrap_or(0);
                            Some((edge, target_idx))
                        }
                        None => {
                            // LLM didn't pick a valid target — treat as terminal
                            None
                        }
                    }
                }
            }
        }
    }
}

/// Prompt the LLM to choose between multiple transition edges.
async fn prompt_llm_transition(
    stage: &Stage,
    edges: &[(&String, &TransitionEdge)],
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
) -> Option<TransitionEdge> {
    // Build the transition prompt
    let prompt = if let Some(ref custom_prompt) = stage.transition_prompt {
        let mut p = custom_prompt.clone();
        p.push_str("\n\nAvailable transitions:\n");
        for (name, edge) in edges {
            p.push_str(&format!("- {}", name));
            if let Some(ref hint) = edge.hint {
                p.push_str(&format!(": {}", hint));
            }
            p.push('\n');
        }
        p.push_str("\nRespond with ONLY the stage name you want to transition to, nothing else.");
        p
    } else {
        let mut p = format!(
            "Stage '{}' is complete. Available next stages:\n",
            stage.name
        );
        for (name, edge) in edges {
            p.push_str(&format!("- {}", name));
            if let Some(ref hint) = edge.hint {
                p.push_str(&format!(": {}", hint));
            }
            p.push('\n');
        }
        p.push_str("\nWhich stage should run next? Respond with ONLY the stage name.");
        p
    };

    // Inject the transition prompt into the context window
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        let tokens = prompt.len() / 4 + 1;
        let _ = window.add_to_region("conversation", format!("User: {}", prompt), tokens);
    }

    // Run a single inference call to get the LLM's choice
    let provider = engine.get_provider(provider_name)?;
    let (messages, max_tokens) = {
        let window = engine.world().get::<ContextWindow>(entity)?;
        let messages = window.assemble_messages();
        let remaining = window.max_tokens.saturating_sub(window.current_tokens);
        let max_tokens = remaining.min(256); // short response expected
        (messages, max_tokens)
    };

    let temperature = 0.0; // deterministic for routing

    let request = leviath_providers::InferenceRequest {
        messages,
        model: model_name.to_string(),
        max_tokens,
        temperature,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
    };

    let response = provider.infer(request).await.ok()?;
    let choice = response.content.trim().to_string();

    // Add the LLM's response to context
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        let tokens = choice.len() / 4 + 1;
        let _ = window.add_to_region(
            "conversation",
            format!("Assistant: Transitioning to: {}", choice),
            tokens,
        );
    }

    // Match the response to an available edge
    for (name, edge) in edges {
        if choice.eq_ignore_ascii_case(name) || choice.contains(name.as_str()) {
            return Some((*edge).clone());
        }
    }

    // Fuzzy fallback: check if any edge target is contained in the response
    for (name, edge) in edges {
        if choice.to_lowercase().contains(&name.to_lowercase()) {
            return Some((*edge).clone());
        }
    }

    // If nothing matched, pick the first edge as fallback
    tracing::warn!(
        stage = %stage.name,
        llm_response = %choice,
        "LLM transition response didn't match any edge — using first available"
    );
    Some(edges.first()?.1.clone())
}

/// Apply an edge transform to the context window before entering the next stage.
async fn apply_edge_transform(
    edge: &TransitionEdge,
    visit_counts: &HashMap<String, usize>,
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    compaction_config: Option<&CompactionConfig>,
) {
    let visits = visit_counts.get(&edge.target).copied().unwrap_or(0);

    // Default: Direct for first visit, Compact for revisits (when no explicit transform)
    let effective_transform = match &edge.transform {
        EdgeTransform::Direct if visits > 0 => {
            // Revisit with no explicit transform: use compact
            &EdgeTransform::Compact { prompt: None }
        }
        other => other,
    };

    match effective_transform {
        EdgeTransform::Direct => {
            // No-op: context carries forward as-is
        }
        EdgeTransform::Clear => {
            // Clear all non-pinned regions
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                for region in &mut window.regions {
                    if !matches!(
                        region.kind,
                        RegionKind::Pinned | RegionKind::CompactHistory { .. }
                    ) {
                        region.clear();
                    }
                }
                window.current_tokens = window.calculate_tokens();
            }
        }
        EdgeTransform::Compact { prompt } => {
            // LLM-summarize conversation/scratch regions
            let compact_prompt = prompt.clone().unwrap_or_else(|| {
                format!(
                    "Summarize the conversation so far as context for the next stage '{}'. \
                     Keep key decisions, findings, and action items. Be concise.",
                    edge.target
                )
            });
            apply_compact_transform(
                engine,
                entity,
                provider_name,
                model_name,
                &compact_prompt,
                compaction_config,
            )
            .await;
        }
        EdgeTransform::Custom {
            carry: _,
            compact,
            clear,
            compact_prompt,
        } => {
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                // Clear specified regions
                for region in &mut window.regions {
                    if clear.contains(&region.name) {
                        region.clear();
                    }
                }
                window.current_tokens = window.calculate_tokens();
            }

            // Compact specified regions
            if !compact.is_empty() {
                let prompt = compact_prompt.clone().unwrap_or_else(|| {
                    format!(
                        "Summarize the content from regions [{}] as context for stage '{}'.",
                        compact.join(", "),
                        edge.target
                    )
                });
                apply_compact_transform(
                    engine,
                    entity,
                    provider_name,
                    model_name,
                    &prompt,
                    compaction_config,
                )
                .await;
            }

            // carry regions are left untouched (they carry forward as-is)
        }
    }
}

/// Run LLM compaction on the conversation/compacting regions.
async fn apply_compact_transform(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    prompt: &str,
    compaction_config: Option<&CompactionConfig>,
) {
    // Use the compaction provider/model if configured, otherwise fall back to the stage's
    let (compact_provider, compact_model) = if let Some(cc) = compaction_config {
        (cc.provider.as_str(), cc.model.as_str())
    } else {
        (provider_name, model_name)
    };

    let provider = match engine.get_provider(compact_provider) {
        Some(p) => p,
        None => return,
    };

    // Gather content from compactable regions
    let content_to_compact = {
        let window = match engine.world().get::<ContextWindow>(entity) {
            Some(w) => w,
            None => return,
        };
        let mut parts = Vec::new();
        for region in &window.regions {
            if matches!(
                region.kind,
                RegionKind::SlidingWindow { .. }
                    | RegionKind::Compacting { .. }
                    | RegionKind::Temporary
                    | RegionKind::Clearable
            ) && !region.content.is_empty()
            {
                let region_content: String = region
                    .content
                    .iter()
                    .map(|e| e.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                parts.push(format!("[{}]:\n{}", region.name, region_content));
            }
        }
        parts.join("\n\n")
    };

    if content_to_compact.is_empty() {
        return;
    }

    let messages = vec![
        leviath_providers::Message {
            role: "system".to_string(),
            content: prompt.to_string(),
        },
        leviath_providers::Message {
            role: "user".to_string(),
            content: content_to_compact,
        },
    ];

    let max_summary_tokens = compaction_config
        .map(|cc| cc.max_summary_tokens)
        .unwrap_or(2000);

    let request = leviath_providers::InferenceRequest {
        messages,
        model: compact_model.to_string(),
        max_tokens: max_summary_tokens,
        temperature: compaction_config.map(|cc| cc.temperature).unwrap_or(0.3),
        tools: Vec::new(),
        extra: serde_json::Value::Null,
    };

    match provider.infer(request).await {
        Ok(response) => {
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                // Clear compactable regions
                for region in &mut window.regions {
                    if matches!(
                        region.kind,
                        RegionKind::SlidingWindow { .. }
                            | RegionKind::Compacting { .. }
                            | RegionKind::Temporary
                            | RegionKind::Clearable
                    ) {
                        region.clear();
                    }
                }
                // Add the summary to conversation
                let tokens = response.content.len() / 4 + 1;
                let _ = window.add_to_region(
                    "conversation",
                    format!(
                        "[Context summary from previous stage]: {}",
                        response.content
                    ),
                    tokens,
                );
                window.current_tokens = window.calculate_tokens();
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to compact context during edge transform");
        }
    }
}

/// Run an agent in the foreground (inline, blocking) — the original behavior.
async fn run_foreground(args: RunArgs) -> anyhow::Result<()> {
    let path = args.path.unwrap_or_else(|| ".".to_string());

    let manifest_path = find_manifest(&path)?;
    println!("Loading agent from: {}", manifest_path.display());

    let manifest_content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("Failed to read manifest: {}", e))?;
    let blueprint = parse_manifest(&manifest_content)?;

    let description = Some(blueprint.description.as_str());
    let task = resolve_task(&args.task, &blueprint.name, description)?;

    tracing::info!(path = %path, task = %task, "Running agent (foreground)");

    println!("Agent: {} v{}", blueprint.name, blueprint.version);
    println!("Task: {}", task);

    let config = Config::load()?;
    for warning in config.validate_keys() {
        println!("Warning: {}", warning);
    }

    let registry = build_provider_registry(&config);
    let mut engine = AgentEngine::with_providers(registry);

    let mut pool = AgentPool::new(blueprint.clone());
    let agent_id = pool.spawn_agent(engine.world_mut());
    let entity = pool
        .get_agent(&agent_id)
        .ok_or_else(|| anyhow::anyhow!("Failed to get spawned agent entity"))?;

    let workdir = std::env::current_dir()?;
    initialize_context_window(&mut engine, entity, &blueprint, &task);

    let tool_registry = Arc::new(ToolRegistry::build(workdir, &config).await);

    // Build launch-level tool policy overrides from CLI flags
    let mut launch_overrides: std::collections::HashMap<String, ToolPolicy> =
        std::collections::HashMap::new();
    if args.yolo {
        launch_overrides.insert("*".to_string(), ToolPolicy::Allow);
    }
    for t in &args.allow {
        launch_overrides.insert(t.clone(), ToolPolicy::Allow);
    }
    for t in &args.ask {
        launch_overrides.insert(t.clone(), ToolPolicy::Ask);
    }
    for t in &args.deny {
        launch_overrides.insert(t.clone(), ToolPolicy::Deny);
    }

    // Session-level tool allows (populated when user chooses "Allow for this session")
    let session_allows: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    // Current stage's permissions (updated per stage below)
    let current_stage_perms: Arc<Mutex<std::collections::HashMap<String, String>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    // Agent-level permissions from the blueprint's [tool_permissions] section
    let agent_perms: std::collections::HashMap<String, String> = blueprint
        .metadata
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix("tool_perm:")
                .and_then(|tool| v.as_str().map(|p| (tool.to_string(), p.to_string())))
        })
        .collect();
    let agent_perms_arc = Arc::new(agent_perms);
    let global_perms = config.tool_permissions.clone();

    // Current stage name for present_for_review interactions (updated per stage below)
    let current_stage_name: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    // Build executor closure (Arcs cloned once here, then again per call)
    let builtins = tool_registry.builtins.clone();
    let mcp = tool_registry.mcp.clone();
    let builtin_names = tool_registry.builtin_names.clone();
    let launch_overrides_arc = Arc::new(launch_overrides);
    let exec_session_allows = session_allows.clone();
    let exec_stage_perms = current_stage_perms.clone();
    let exec_stage_name = current_stage_name.clone();
    let exec_agent_perms = agent_perms_arc.clone();
    let exec_global_perms = Arc::new(global_perms);
    let mut exec = move |calls: Vec<leviath_providers::ToolCall>| {
        let builtins = builtins.clone();
        let mcp = mcp.clone();
        let builtin_names = builtin_names.clone();
        let launch_ov = launch_overrides_arc.clone();
        let session_al = exec_session_allows.clone();
        let stage_pm = exec_stage_perms.clone();
        let stage_nm = exec_stage_name.clone();
        let agent_pm = exec_agent_perms.clone();
        let global_pm = exec_global_perms.clone();
        async move {
            let mut out: Vec<(String, String)> = Vec::new();
            for tc in calls {
                // ── present_for_review: print document to stdout, ask for feedback ──
                if tc.name == "present_for_review" {
                    let title = tc
                        .arguments
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Review")
                        .to_string();
                    let markdown = tc
                        .arguments
                        .get("markdown")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let stage_name = stage_nm.lock().await.clone();

                    println!("\n{}", "─".repeat(60));
                    println!("  {}", title);
                    println!("{}", "─".repeat(60));
                    println!("{}", markdown);
                    println!("{}", "─".repeat(60));

                    use crate::interaction::{
                        request_interaction_stdin, response_as_text, InteractionRequest,
                    };
                    let req = InteractionRequest::review(
                        format!("fg-review-{}", tc.id),
                        &title,
                        &markdown,
                        &stage_name,
                    );
                    let resp = request_interaction_stdin(&req);
                    let user_feedback = response_as_text(&resp);
                    let result = if user_feedback.trim().is_empty() {
                        "User reviewed the document and acknowledged.".to_string()
                    } else {
                        format!("User feedback: {}", user_feedback)
                    };
                    out.push((tc.id.clone(), result));
                    continue;
                }

                let is_builtin = builtin_names.contains(&tc.name);
                let session_has = session_al.lock().await.contains(&tc.name);
                let policy = if session_has {
                    ToolPolicy::Allow
                } else {
                    let stage_pm_snap = stage_pm.lock().await.clone();
                    resolve_policy(
                        &tc.name,
                        is_builtin,
                        &launch_ov,
                        &stage_pm_snap,
                        &agent_pm,
                        &global_pm,
                    )
                };

                let res = match policy {
                    ToolPolicy::Deny => {
                        format!("[denied] Tool '{}' is not permitted for this run.", tc.name)
                    }
                    ToolPolicy::Ask => {
                        // Foreground: ask via stdin
                        use crate::interaction::{
                            request_interaction_stdin, response_approved, ApprovalScope,
                            InteractionRequest,
                        };
                        let req = InteractionRequest::tool_approval(
                            format!("fg-{}", tc.id),
                            &tc.name,
                            tc.arguments.clone(),
                            "tool-call",
                        );
                        let resp = request_interaction_stdin(&req);
                        if response_approved(&resp) {
                            if resp.scope == Some(ApprovalScope::Session) {
                                session_al.lock().await.insert(tc.name.clone());
                            }
                            if is_builtin {
                                builtins.execute(&tc.name, tc.arguments.clone()).await
                            } else {
                                let mut mcp_lock = mcp.lock().await;
                                match mcp_lock.execute(&tc.name, tc.arguments.clone()).await {
                                    Ok(r) if r.success => r.text,
                                    Ok(r) => format!("[error] {}", r.text),
                                    Err(e) => format!("[error] tool error: {}", e),
                                }
                            }
                        } else {
                            format!("[denied] User declined tool call '{}'.", tc.name)
                        }
                    }
                    ToolPolicy::Allow => {
                        if is_builtin {
                            builtins.execute(&tc.name, tc.arguments.clone()).await
                        } else {
                            let mut mcp_lock = mcp.lock().await;
                            match mcp_lock.execute(&tc.name, tc.arguments.clone()).await {
                                Ok(r) if r.success => r.text,
                                Ok(r) => format!("[error] {}", r.text),
                                Err(e) => format!("[error] tool error: {}", e),
                            }
                        }
                    }
                };
                out.push((tc.id.clone(), res));
            }
            out
        }
    };

    let compaction_config = blueprint.compaction_config.clone();
    let compaction_ref = compaction_config.as_ref();

    // ─── Graph stage loop ────────────────────────────────────────────────────
    let entry_name = blueprint.resolve_entry_stage_name();
    let mut current_stage_name_val = entry_name;
    let mut current_stage_idx = blueprint
        .stages
        .iter()
        .position(|s| s.name == current_stage_name_val)
        .unwrap_or(0);
    let mut visit_counts: HashMap<String, usize> = HashMap::new();

    loop {
        let stage = blueprint
            .find_stage(&current_stage_name_val)
            .ok_or_else(|| anyhow::anyhow!("Stage '{}' not found", current_stage_name_val))?;

        let provider_name = &stage.model.provider;
        let model_name = args.model.as_deref().unwrap_or(&stage.model.model);

        // Update current stage permissions and name for the executor closure
        {
            let mut sp = current_stage_perms.lock().await;
            *sp = stage.tool_permissions.clone();
        }
        {
            let mut sn = current_stage_name.lock().await;
            *sn = stage.name.clone();
        }

        if !engine.providers().has(provider_name) {
            println!(
                "\nProvider '{}' is not configured. Please set an API key in ~/.leviath/config.toml",
                provider_name
            );
            println!("\nExample config:");
            println!("  [providers]");
            println!("  anthropic_api_key = \"sk-ant-...\"");
            println!("\nOr set the ANTHROPIC_API_KEY environment variable.");
            println!("\nOr use Claude Code (no API key needed):");
            println!("  [stages.main]");
            println!("  model = {{ provider = \"claude-code\", model = \"claude-sonnet-4-6\" }}");
            return Ok(());
        }

        let visit_num = visit_counts.get(&stage.name).copied().unwrap_or(0);
        let visit_label = if visit_num > 0 {
            format!(" (visit {})", visit_num + 1)
        } else {
            String::new()
        };
        println!(
            "\n--- Stage {}: {} ({}:{}){} ---",
            current_stage_idx + 1,
            stage.name,
            provider_name,
            model_name,
            visit_label,
        );

        if provider_name == "claude-code" {
            println!("⚠️  This stage uses the claude-code provider.");
            println!("   Tool routing, per-stage filtering, and prompt caching are not available.");
            println!("   For full features, use provider = \"anthropic\" with an API key.");
            println!();
        }

        if let Some(ref stage_layout) = stage.context_layout {
            swap_context_layout(&mut engine, entity, stage_layout);
        }

        // Inject per-stage system prompt into context
        if let Some(sp) = stage.config.get("system_prompt").and_then(|v| v.as_str()) {
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = sp.len() / 4 + 1;
                let _ = window.add_to_region(
                    "conversation",
                    format!("[Stage instructions: {}]", sp),
                    tokens,
                );
            }
        }

        let all_tools = tool_registry.all_tool_defs();
        let effective_tools: Vec<leviath_providers::Tool> = if stage.available_tools.is_empty() {
            Vec::new()
        } else {
            all_tools
                .into_iter()
                .filter(|t| stage.available_tools.iter().any(|f| f == &t.name))
                .collect()
        };

        let routing_config =
            stage
                .tool_result_routing
                .as_ref()
                .map(|r| leviath_runtime::ToolResultRoutingConfig {
                    default_region: r.default_region.clone(),
                    tool_overrides: r.tool_overrides.clone(),
                    persist: r.persist,
                    max_result_tokens: r.max_result_tokens,
                });
        let routing_ref = routing_config.as_ref();
        let max_iterations = stage.max_iterations.unwrap_or(20);

        // Determine stage result for transition condition evaluation
        let stage_result_val: StageResult;

        match &stage.mode {
            StageMode::Interactive => {
                run_interactive_stage(
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    &effective_tools,
                    None, // foreground — use stdin
                    &stage.name,
                    &mut exec,
                )
                .await?;
                stage_result_val = StageResult::Success;
            }
            StageMode::InteractivePoints { points } => {
                run_interactive_points_stage(
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    &effective_tools,
                    routing_ref,
                    compaction_ref,
                    points,
                    None, // foreground — use stdin
                    &mut exec,
                )
                .await?;
                stage_result_val = StageResult::Success;
            }
            StageMode::Autonomous => {
                match run_autonomous_stage(
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    &effective_tools,
                    routing_ref,
                    compaction_ref,
                    &mut exec,
                )
                .await
                {
                    Ok(()) => {
                        // Check if we hit max_iterations by looking at iteration count
                        if let Some(state) = engine.world().get::<AgentState>(entity) {
                            if stage.max_iterations.is_some() && state.iteration >= max_iterations {
                                stage_result_val = StageResult::MaxIterations;
                            } else {
                                stage_result_val = StageResult::Success;
                            }
                        } else {
                            stage_result_val = StageResult::Success;
                        }
                    }
                    Err(e) => {
                        // Check if error edges exist; if so, route to error handler
                        if is_graph_mode(&blueprint) {
                            println!("Stage error: {} — checking error transitions", e);
                            stage_result_val = StageResult::Error;
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
        }

        *visit_counts.entry(stage.name.clone()).or_default() += 1;

        // Resolve the next transition
        // We need to clone the stage name before passing blueprint as &mut
        let stage_name_owned = current_stage_name_val.clone();
        let stage_ref = blueprint.find_stage(&stage_name_owned).unwrap();
        let transition = resolve_transition(
            stage_ref,
            current_stage_idx,
            &blueprint,
            &visit_counts,
            &stage_result_val,
            &mut engine,
            entity,
            provider_name,
            model_name,
        )
        .await;

        match transition {
            Some((edge, next_idx)) => {
                let next_name = edge.target.clone();
                if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                    let marker = format!(
                        "[Stage complete: {}, transitioning to: {}]",
                        stage_name_owned, next_name
                    );
                    let tokens = marker.len() / 4 + 1;
                    let _ = window.add_to_region("conversation", marker, tokens);
                }

                // Apply edge transform
                apply_edge_transform(
                    &edge,
                    &visit_counts,
                    &mut engine,
                    entity,
                    &edge.target, // use target's provider — but we'll use current for simplicity
                    model_name,
                    compaction_ref,
                )
                .await;

                current_stage_name_val = next_name;
                current_stage_idx = next_idx;
            }
            None => {
                break; // terminal: no valid transitions
            }
        }
    }

    println!("\n[All stages complete]");
    tool_registry.shutdown().await;
    Ok(())
}

/// Background worker entrypoint: runs stages and writes progress to run-state dir.
pub async fn execute_worker(args: WorkerArgs) -> anyhow::Result<()> {
    let mut meta = runstate::read_meta(&args.run_id).unwrap_or_else(|_| {
        RunMeta::new(
            args.run_id.clone(),
            "unknown".to_string(),
            args.path.clone(),
            args.task.clone(),
            args.model.clone(),
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            0,
        )
    });

    meta.pid = std::process::id();
    meta.status = RunStatus::Running;
    meta.touch();
    let _ = runstate::write_meta(&meta);

    let result = run_worker_inner(&args, &mut meta).await;

    match &result {
        Ok(()) => meta.status = RunStatus::Complete,
        Err(e) => {
            meta.status = RunStatus::Error;
            meta.error = Some(e.to_string());
        }
    }
    meta.touch();
    let _ = runstate::write_meta(&meta);

    result
}

async fn run_worker_inner(args: &WorkerArgs, meta: &mut RunMeta) -> anyhow::Result<()> {
    let manifest_path = find_manifest(&args.path)?;
    println!("Loading agent from: {}", manifest_path.display());

    let manifest_content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("Failed to read manifest: {}", e))?;
    let blueprint = parse_manifest(&manifest_content)?;

    println!("Agent: {} v{}", blueprint.name, blueprint.version);
    println!("Task: {}", args.task);

    let config = Config::load()?;
    for warning in config.validate_keys() {
        println!("Warning: {}", warning);
    }

    let prov_registry = build_provider_registry(&config);

    // Generate a human-readable title from the task prompt (best-effort).
    if config.title.enabled && meta.title.is_none() {
        let fallback = args.model.as_deref();
        meta.title = generate_title(&args.task, &config, &prov_registry, fallback).await;
        if let Some(ref t) = meta.title {
            println!("Title: {}", t);
        }
        meta.touch();
        let _ = runstate::write_meta(meta);
    }

    let mut engine = AgentEngine::with_providers(prov_registry);

    let mut pool = AgentPool::new(blueprint.clone());
    let agent_id = pool.spawn_agent(engine.world_mut());
    let entity = pool
        .get_agent(&agent_id)
        .ok_or_else(|| anyhow::anyhow!("Failed to get spawned agent entity"))?;

    let workdir = std::env::current_dir()?;
    initialize_context_window(&mut engine, entity, &blueprint, &args.task);

    let tool_registry = Arc::new(ToolRegistry::build(workdir, &config).await);

    // Global tool policy + session-level allows
    let global_perms = Arc::new(config.tool_permissions.clone());
    let session_allows: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    let current_stage_perms: Arc<Mutex<std::collections::HashMap<String, String>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    // Agent-level permissions from the blueprint's [tool_permissions] section
    let agent_perms: std::collections::HashMap<String, String> = blueprint
        .metadata
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix("tool_perm:")
                .and_then(|tool| v.as_str().map(|p| (tool.to_string(), p.to_string())))
        })
        .collect();
    let agent_perms_arc = Arc::new(agent_perms);
    // Launch overrides forwarded from the CLI flags
    let mut launch_overrides: std::collections::HashMap<String, ToolPolicy> =
        std::collections::HashMap::new();
    if args.yolo {
        launch_overrides.insert("*".to_string(), ToolPolicy::Allow);
    }
    for t in &args.allow {
        launch_overrides.insert(t.clone(), ToolPolicy::Allow);
    }
    for t in &args.ask {
        launch_overrides.insert(t.clone(), ToolPolicy::Ask);
    }
    for t in &args.deny {
        launch_overrides.insert(t.clone(), ToolPolicy::Deny);
    }
    let launch_overrides_arc: Arc<std::collections::HashMap<String, ToolPolicy>> =
        Arc::new(launch_overrides);
    let run_id_arc = Arc::new(args.run_id.clone());
    // Shared mutable stage index so the executor closure can log tool activity
    // to the correct per-stage log file.
    let current_stage_idx: CurrentStageIdx = Arc::new(Mutex::new(0usize));
    // Shared current stage name for present_for_review interactions.
    let current_stage_name: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let builtins = tool_registry.builtins.clone();
    let mcp = tool_registry.mcp.clone();
    let builtin_names = tool_registry.builtin_names.clone();
    let exec_session_allows = session_allows.clone();
    let exec_stage_perms = current_stage_perms.clone();
    let exec_agent_perms = agent_perms_arc.clone();
    let exec_global_perms = global_perms.clone();
    let exec_run_id = run_id_arc.clone();
    let exec_stage_idx = current_stage_idx.clone();
    let exec_stage_name = current_stage_name.clone();
    let mut exec = move |calls: Vec<leviath_providers::ToolCall>| {
        let builtins = builtins.clone();
        let mcp = mcp.clone();
        let builtin_names = builtin_names.clone();
        let launch_ov = launch_overrides_arc.clone();
        let session_al = exec_session_allows.clone();
        let stage_pm = exec_stage_perms.clone();
        let agent_pm = exec_agent_perms.clone();
        let global_pm = exec_global_perms.clone();
        let run_id = exec_run_id.clone();
        let stage_idx_arc = exec_stage_idx.clone();
        let stage_name_arc = exec_stage_name.clone();
        async move {
            let stage_idx = *stage_idx_arc.lock().await;
            let stage_name = stage_name_arc.lock().await.clone();
            let mut out: Vec<(String, String)> = Vec::new();
            for tc in calls {
                // ── present_for_review: special built-in that raises an interaction ──
                if tc.name == "present_for_review" {
                    let title = tc
                        .arguments
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Review")
                        .to_string();
                    let markdown = tc
                        .arguments
                        .get("markdown")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    // Persist the review artifact under stages/<idx>/reviews/
                    let review_dir = runstate::stage_dir(&run_id, stage_idx).join("reviews");
                    let _ = std::fs::create_dir_all(&review_dir);
                    let artifact_path = review_dir.join(format!("review-{}.md", tc.id));
                    let _ = std::fs::write(&artifact_path, &markdown);

                    // Also write to stage output so it's visible in the Output tab after review
                    record_stage_output(
                        &run_id,
                        stage_idx,
                        &format!("---\n## {}\n\n{}\n---", title, markdown),
                    );

                    // Log the event
                    record_stage_log(
                        &run_id,
                        stage_idx,
                        &format!(
                            "[tool] present_for_review → waiting for user review: {}",
                            title
                        ),
                    );

                    // Build the interaction request with markdown body
                    let req = crate::interaction::InteractionRequest::review(
                        format!("review-{}", tc.id),
                        &title,
                        &markdown,
                        &stage_name,
                    );

                    // Write request and wait for response
                    let resp =
                        crate::interaction::request_interaction_bg_review(&run_id, req).await;

                    let user_feedback = crate::interaction::response_as_text(&resp);
                    let result = if user_feedback.trim().is_empty() {
                        "User reviewed the document and acknowledged.".to_string()
                    } else {
                        format!("User feedback: {}", user_feedback)
                    };
                    record_stage_log(&run_id, stage_idx, "[tool] present_for_review → done");
                    out.push((tc.id.clone(), result));
                    continue;
                }

                let is_builtin = builtin_names.contains(&tc.name);
                let session_has = session_al.lock().await.contains(&tc.name);
                let policy = if session_has {
                    ToolPolicy::Allow
                } else {
                    let stage_pm_snap = stage_pm.lock().await.clone();
                    resolve_policy(
                        &tc.name,
                        is_builtin,
                        &launch_ov,
                        &stage_pm_snap,
                        &agent_pm,
                        &global_pm,
                    )
                };

                let res = match policy {
                    ToolPolicy::Deny => {
                        let msg = format!("[denied] Tool '{}' is not permitted.", tc.name);
                        record_stage_log(
                            &run_id,
                            stage_idx,
                            &format!("[tool] {} → denied", tc.name),
                        );
                        msg
                    }
                    ToolPolicy::Ask => {
                        use crate::interaction::{request_tool_approval_background, ApprovalScope};
                        let (approved, scope) = request_tool_approval_background(
                            &run_id,
                            &tc.name,
                            &tc.arguments,
                            "tool-call",
                        )
                        .await;
                        if approved {
                            if scope == ApprovalScope::Session {
                                session_al.lock().await.insert(tc.name.clone());
                            }
                            let result = if is_builtin {
                                builtins.execute(&tc.name, tc.arguments.clone()).await
                            } else {
                                let mut mcp_lock = mcp.lock().await;
                                match mcp_lock.execute(&tc.name, tc.arguments.clone()).await {
                                    Ok(r) if r.success => r.text,
                                    Ok(r) => format!("[error] {}", r.text),
                                    Err(e) => format!("[error] tool error: {}", e),
                                }
                            };
                            let short_result = if result.len() > 120 {
                                format!("{}…", &result[..120])
                            } else {
                                result.clone()
                            };
                            record_stage_log(
                                &run_id,
                                stage_idx,
                                &format!("[tool] {} → {}", tc.name, short_result),
                            );
                            result
                        } else {
                            record_stage_log(
                                &run_id,
                                stage_idx,
                                &format!("[tool] {} → declined by user", tc.name),
                            );
                            format!("[denied] User declined tool call '{}'.", tc.name)
                        }
                    }
                    ToolPolicy::Allow => {
                        let result = if is_builtin {
                            builtins.execute(&tc.name, tc.arguments.clone()).await
                        } else {
                            let mut mcp_lock = mcp.lock().await;
                            match mcp_lock.execute(&tc.name, tc.arguments.clone()).await {
                                Ok(r) if r.success => r.text,
                                Ok(r) => format!("[error] {}", r.text),
                                Err(e) => format!("[error] tool error: {}", e),
                            }
                        };
                        let short_result = if result.len() > 120 {
                            format!("{}…", &result[..120])
                        } else {
                            result.clone()
                        };
                        record_stage_log(
                            &run_id,
                            stage_idx,
                            &format!("[tool] {} → {}", tc.name, short_result),
                        );
                        result
                    }
                };
                out.push((tc.id.clone(), res));
            }
            out
        }
    };

    let compaction_config = blueprint.compaction_config.clone();
    let compaction_ref = compaction_config.as_ref();

    meta.num_stages = blueprint.stages.len();
    let _ = runstate::write_meta(meta);

    // Initialize the stages index (all Pending) so the dashboard can show stages
    // before any stage starts running.
    {
        let initial_stages: Vec<StageRecord> = blueprint
            .stages
            .iter()
            .enumerate()
            .map(|(i, s)| StageRecord::new(s.name.clone(), i))
            .collect();
        let _ = runstate::write_stages_index(&args.run_id, &initial_stages);
    }

    // ─── Graph stage loop (worker) ──────────────────────────────────────────
    let entry_name = blueprint.resolve_entry_stage_name();
    let mut current_stage_name_val = entry_name;
    let mut current_stage_idx_val = blueprint
        .stages
        .iter()
        .position(|s| s.name == current_stage_name_val)
        .unwrap_or(0);
    let mut visit_counts: HashMap<String, usize> = HashMap::new();

    loop {
        let stage = blueprint
            .find_stage(&current_stage_name_val)
            .ok_or_else(|| anyhow::anyhow!("Stage '{}' not found", current_stage_name_val))?;

        let stage_idx = current_stage_idx_val;
        let provider_name = &stage.model.provider;
        let model_name = args.model.as_deref().unwrap_or(&stage.model.model);

        // Update current stage permissions + index + name for the executor closure
        {
            let mut sp = current_stage_perms.lock().await;
            *sp = stage.tool_permissions.clone();
        }
        {
            let mut si = current_stage_idx.lock().await;
            *si = stage_idx;
        }
        {
            let mut sn = current_stage_name.lock().await;
            *sn = stage.name.clone();
        }

        if !engine.providers().has(provider_name) {
            let msg = format!("Provider '{}' is not configured", provider_name);
            println!("\n{}", msg);
            record_stage_log(&args.run_id, stage_idx, &format!("[error] {}", msg));
            {
                let mut stages = runstate::read_stages_index(&args.run_id);
                if let Some(r) = stages.get_mut(stage_idx) {
                    r.status = StageRunStatus::Error;
                }
                let _ = runstate::write_stages_index(&args.run_id, &stages);
            }
            meta.status = RunStatus::Error;
            meta.error = Some(msg);
            meta.touch();
            let _ = runstate::write_meta(meta);
            return Ok(());
        }

        let visit_num = visit_counts.get(&stage.name).copied().unwrap_or(0);
        let visit_label = if visit_num > 0 {
            format!(" (visit {})", visit_num + 1)
        } else {
            String::new()
        };
        let stage_header = format!(
            "Stage {}: {} ({}:{}){}",
            stage_idx + 1,
            stage.name,
            provider_name,
            model_name,
            visit_label,
        );
        println!("\n--- {} ---", stage_header);
        record_stage_log(
            &args.run_id,
            stage_idx,
            &format!("--- {} ---", stage_header),
        );

        if provider_name == "claude-code" {
            let warn = "⚠️  Using claude-code provider: tool routing, per-stage filtering, and prompt caching are not available.";
            println!("{}", warn);
            record_stage_log(&args.run_id, stage_idx, warn);
        }

        // Mark stage as active and update stages.json
        let stage_started_at = {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        };
        {
            let mut stages = runstate::read_stages_index(&args.run_id);
            if let Some(r) = stages.get_mut(stage_idx) {
                r.status = StageRunStatus::Active;
                r.started_at = Some(stage_started_at);
            }
            let _ = runstate::write_stages_index(&args.run_id, &stages);
        }

        meta.current_stage = stage.name.clone();
        meta.stage_index = stage_idx;
        meta.status = RunStatus::Running;
        meta.touch();
        let _ = runstate::write_meta(meta);

        if let Some(ref stage_layout) = stage.context_layout {
            swap_context_layout(&mut engine, entity, stage_layout);
        }

        // Inject per-stage system prompt into context
        if let Some(sp) = stage.config.get("system_prompt").and_then(|v| v.as_str()) {
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = sp.len() / 4 + 1;
                let _ = window.add_to_region(
                    "conversation",
                    format!("[Stage instructions: {}]", sp),
                    tokens,
                );
            }
        }

        let all_tools = tool_registry.all_tool_defs();
        let effective_tools: Vec<leviath_providers::Tool> = if stage.available_tools.is_empty() {
            Vec::new()
        } else {
            all_tools
                .into_iter()
                .filter(|t| stage.available_tools.iter().any(|f| f == &t.name))
                .collect()
        };

        let routing_config =
            stage
                .tool_result_routing
                .as_ref()
                .map(|r| leviath_runtime::ToolResultRoutingConfig {
                    default_region: r.default_region.clone(),
                    tool_overrides: r.tool_overrides.clone(),
                    persist: r.persist,
                    max_result_tokens: r.max_result_tokens,
                });
        let routing_ref = routing_config.as_ref();
        let max_iterations = stage.max_iterations.unwrap_or(20);

        // Determine stage result for transition condition evaluation
        let stage_result_val: StageResult;

        // Workers now support interactive stage modes via the file-based IPC channel.
        let stage_run_result: anyhow::Result<Option<leviath_providers::InferenceResponse>> =
            match &stage.mode {
                StageMode::Interactive => run_interactive_stage(
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    &effective_tools,
                    Some((&args.run_id, meta)),
                    &stage.name,
                    &mut exec,
                )
                .await
                .map(|_| None),
                StageMode::InteractivePoints { points } => {
                    let pts = points.clone();
                    run_interactive_points_stage(
                        &mut engine,
                        entity,
                        provider_name,
                        model_name,
                        max_iterations,
                        &effective_tools,
                        routing_ref,
                        compaction_ref,
                        &pts,
                        Some((&args.run_id, meta)),
                        &mut exec,
                    )
                    .await
                    .map(|_| None)
                }
                StageMode::Autonomous => engine
                    .run_inference_loop_filtered(
                        entity,
                        provider_name,
                        model_name,
                        effective_tools,
                        max_iterations,
                        None,
                        routing_ref,
                        compaction_ref,
                        &mut exec,
                    )
                    .await
                    .map(Some)
                    .map_err(|e| anyhow::anyhow!("{}", e)),
            };

        let stage_ended_at = {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        };

        match stage_run_result {
            Ok(resp_opt) => {
                if let Some(resp) = resp_opt {
                    // Route the readable agent response to both stdout (legacy) and per-stage output
                    println!("{}", resp.content);
                    record_stage_output(&args.run_id, stage_idx, &resp.content);

                    let token_line = format!(
                        "[Tokens: {} in, {} out]",
                        resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
                    );
                    println!("\n{}", token_line);
                    record_stage_log(&args.run_id, stage_idx, &token_line);

                    meta.prompt_tokens += resp.tokens_used.prompt_tokens;
                    meta.completion_tokens += resp.tokens_used.completion_tokens;

                    // Carry the final response forward so the next stage sees the previous stage's output
                    if !resp.content.is_empty() {
                        if let Some(mut window) =
                            engine.world_mut().get_mut::<ContextWindow>(entity)
                        {
                            let tokens = resp.content.len() / 4 + 1;
                            let _ = window.add_to_region(
                                "conversation",
                                format!("Assistant ({}): {}", stage.name, resp.content),
                                tokens,
                            );
                        }
                    }
                }

                // Determine if max_iterations was hit
                if let Some(state) = engine.world().get::<AgentState>(entity) {
                    if stage.max_iterations.is_some() && state.iteration >= max_iterations {
                        stage_result_val = StageResult::MaxIterations;
                    } else {
                        stage_result_val = StageResult::Success;
                    }
                } else {
                    stage_result_val = StageResult::Success;
                }

                // Mark stage complete
                {
                    let mut stages = runstate::read_stages_index(&args.run_id);
                    if let Some(r) = stages.get_mut(stage_idx) {
                        r.status = StageRunStatus::Complete;
                        r.ended_at = Some(stage_ended_at);
                        r.prompt_tokens = meta.prompt_tokens;
                        r.completion_tokens = meta.completion_tokens;
                    }
                    let _ = runstate::write_stages_index(&args.run_id, &stages);
                }
            }
            Err(e) => {
                // In graph mode, errors can route to error-handler stages
                if is_graph_mode(&blueprint) {
                    let msg = format!(
                        "Stage '{}' error: {} — checking error transitions",
                        stage.name, e
                    );
                    println!("{}", msg);
                    record_stage_log(&args.run_id, stage_idx, &format!("[error] {}", msg));
                    stage_result_val = StageResult::Error;

                    // Mark stage as errored but don't abort
                    {
                        let mut stages = runstate::read_stages_index(&args.run_id);
                        if let Some(r) = stages.get_mut(stage_idx) {
                            r.status = StageRunStatus::Error;
                            r.ended_at = Some(stage_ended_at);
                        }
                        let _ = runstate::write_stages_index(&args.run_id, &stages);
                    }
                } else {
                    let msg = format!("Stage '{}' inference error: {}", stage.name, e);
                    println!("{}", msg);
                    record_stage_log(&args.run_id, stage_idx, &format!("[error] {}", msg));
                    // Mark stage error
                    {
                        let mut stages = runstate::read_stages_index(&args.run_id);
                        if let Some(r) = stages.get_mut(stage_idx) {
                            r.status = StageRunStatus::Error;
                            r.ended_at = Some(stage_ended_at);
                        }
                        let _ = runstate::write_stages_index(&args.run_id, &stages);
                    }
                    meta.status = RunStatus::Error;
                    meta.error = Some(msg);
                    meta.touch();
                    let _ = runstate::write_meta(meta);
                    return Ok(());
                }
            }
        }

        *visit_counts.entry(stage.name.clone()).or_default() += 1;

        if let Some(state) = engine.world().get::<AgentState>(entity) {
            meta.iteration = state.iteration;
        }
        meta.touch();
        let _ = runstate::write_meta(meta);
        // Write context snapshot to both legacy path and per-stage path
        write_context_snapshot_if_bg(&engine, entity, &stage.name, &Some(args.run_id.clone()));
        if let Some(snap) = build_context_snapshot(&engine, entity, &stage.name) {
            let _ = runstate::write_stage_context(&args.run_id, stage_idx, &snap);
        }

        // Resolve the next transition
        let stage_name_owned = current_stage_name_val.clone();
        let stage_ref = blueprint.find_stage(&stage_name_owned).unwrap();
        let transition = resolve_transition(
            stage_ref,
            current_stage_idx_val,
            &blueprint,
            &visit_counts,
            &stage_result_val,
            &mut engine,
            entity,
            provider_name,
            model_name,
        )
        .await;

        match transition {
            Some((edge, next_idx)) => {
                let next_name = edge.target.clone();
                let marker = format!(
                    "[Stage complete: {}, transitioning to: {}]",
                    stage_name_owned, next_name
                );
                record_stage_log(&args.run_id, stage_idx, &marker);
                if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                    let tokens = marker.len() / 4 + 1;
                    let _ = window.add_to_region("conversation", marker, tokens);
                }

                // Apply edge transform
                apply_edge_transform(
                    &edge,
                    &visit_counts,
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    compaction_ref,
                )
                .await;

                current_stage_name_val = next_name;
                current_stage_idx_val = next_idx;
            }
            None => break, // terminal: no valid transitions
        }
    }

    let done_msg = "[All stages complete]";
    println!("\n{}", done_msg);
    // Log the completion message to the last stage's log
    if !blueprint.stages.is_empty() {
        record_stage_log(&args.run_id, current_stage_idx_val, done_msg);
    }
    tool_registry.shutdown().await;
    Ok(())
}

// ─── Stage runners ───────────────────────────────────────────────────────────

/// Run an interactive stage.
///
/// `run_context`: if `Some((run_id, meta))`, interaction is handled via the
/// file-based IPC channel (background worker). If `None`, stdin is used
/// (foreground).
#[allow(clippy::too_many_arguments)]
async fn run_interactive_stage<F, Fut>(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    run_context: Option<(&str, &mut RunMeta)>,
    stage_name: &str,
    executor: &mut F,
) -> anyhow::Result<()>
where
    F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut,
    Fut: std::future::Future<Output = Vec<(String, String)>>,
{
    use crate::interaction::{
        make_interaction_id, request_interaction_async, response_as_text, InteractionRequest,
    };

    let has_tools = !tools.is_empty();
    let mut turn = 0;

    // We need to hold the run_id separately since we consume run_context's meta
    // across iterations. Decouple them to avoid borrow issues.
    let (run_id_owned, meta_opt): (Option<String>, Option<&mut RunMeta>) = match run_context {
        Some((rid, m)) => (Some(rid.to_string()), Some(m)),
        None => (None, None),
    };

    // We need meta across loop iterations — box it optionally.
    let mut meta_holder = meta_opt;

    loop {
        if turn >= max_iterations {
            println!("\n[Max turns reached]");
            break;
        }

        if has_tools {
            let per_turn_iters = 10_usize.min(max_iterations.saturating_sub(turn));
            let response = engine
                .run_inference_loop_filtered(
                    entity,
                    provider_name,
                    model_name,
                    tools.to_vec(),
                    per_turn_iters,
                    None,
                    None,
                    None,
                    executor,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Inference error: {}", e))?;

            println!("\nAssistant: {}", response.content);
            let token_line = format!(
                "[Tokens: {} in, {} out]",
                response.tokens_used.prompt_tokens, response.tokens_used.completion_tokens
            );
            println!("\n{}", token_line);

            // Route to per-stage files so the dashboard can display them.
            if let (Some(run_id), Some(ref m)) = (&run_id_owned, &meta_holder) {
                record_stage_output(run_id, m.stage_index, &response.content);
                record_stage_log(run_id, m.stage_index, &token_line);
            }

            // Update meta token counts so the dashboard shows them before the
            // next interaction point (before they go to WaitingInput).
            if let Some(ref mut m) = meta_holder {
                m.prompt_tokens += response.tokens_used.prompt_tokens;
                m.completion_tokens += response.tokens_used.completion_tokens;
                m.touch();
                let _ = runstate::write_meta(m);
            }

            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = response.content.len() / 4 + 1;
                let _ = window.add_to_region(
                    "conversation",
                    format!("Assistant: {}", response.content),
                    tokens,
                );
            }
        } else {
            let response = match stream_inference(engine, entity, provider_name, model_name, None)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("Streaming unavailable, falling back: {}", e);
                    let r = engine
                        .run_inference_filtered(entity, provider_name, model_name, Vec::new(), None)
                        .await
                        .map_err(|e| anyhow::anyhow!("Inference error: {}", e))?;
                    println!("\nAssistant: {}", r.content);
                    r
                }
            };

            println!(
                "\n[Tokens: {} in, {} out]",
                response.tokens_used.prompt_tokens, response.tokens_used.completion_tokens
            );

            if let Some(ref mut m) = meta_holder {
                m.prompt_tokens += response.tokens_used.prompt_tokens;
                m.completion_tokens += response.tokens_used.completion_tokens;
                m.touch();
                let _ = runstate::write_meta(m);
            }

            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = response.content.len() / 4 + 1;
                let _ = window.add_to_region(
                    "conversation",
                    format!("Assistant: {}", response.content),
                    tokens,
                );
            }
        }

        // Build and dispatch the input request
        let req = InteractionRequest::free_text(
            make_interaction_id(0, turn),
            "Your response (leave empty or /quit to end):",
            stage_name,
            false, // not required — empty ends the loop
        );

        let input = if let (Some(run_id), Some(ref mut meta)) = (&run_id_owned, &mut meta_holder) {
            let resp = request_interaction_async(run_id, meta, req, None).await?;
            response_as_text(&resp)
        } else {
            crate::interaction::request_interaction_stdin(&req);
            // For stdin, we need to actually read in the FreeText path
            use std::io::Write;
            print!("\nYou: ");
            std::io::stdout().flush().ok();
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            buf.trim().to_string()
        };

        if input.is_empty() || input == "/quit" || input == "/exit" {
            println!("\n[Session ended]");
            break;
        }

        if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
            let tokens = input.len() / 4 + 1;
            let _ = window.add_to_region("conversation", format!("User: {}", input), tokens);
        }

        turn += 1;
    }

    Ok(())
}

/// Run an autonomous stage with the real tool executor.
#[allow(clippy::too_many_arguments)]
async fn run_autonomous_stage<F, Fut>(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    routing: Option<&leviath_runtime::ToolResultRoutingConfig>,
    compaction_config: Option<&CompactionConfig>,
    executor: &mut F,
) -> anyhow::Result<()>
where
    F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut,
    Fut: std::future::Future<Output = Vec<(String, String)>>,
{
    let response = engine
        .run_inference_loop_filtered(
            entity,
            provider_name,
            model_name,
            tools.to_vec(),
            max_iterations,
            None,
            routing,
            compaction_config,
            executor,
        )
        .await;

    match response {
        Ok(resp) => {
            println!("{}", resp.content);
            println!(
                "\n[Tokens used: {} input, {} output]",
                resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
            );
        }
        Err(e) => {
            println!("Inference error: {}", e);
        }
    }
    Ok(())
}

/// Run an InteractivePoints stage: autonomous iterations with pauses at each interaction point.
///
/// `run_context`: if `Some((run_id, meta))`, interaction is handled via the
/// file-based IPC channel (background worker). If `None`, stdin is used
/// (foreground).
#[allow(clippy::too_many_arguments)]
async fn run_interactive_points_stage<F, Fut>(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    routing: Option<&leviath_runtime::ToolResultRoutingConfig>,
    compaction_config: Option<&CompactionConfig>,
    points: &[leviath_core::blueprint::InteractionPoint],
    run_context: Option<(&str, &mut RunMeta)>,
    executor: &mut F,
) -> anyhow::Result<()>
where
    F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut,
    Fut: std::future::Future<Output = Vec<(String, String)>>,
{
    use crate::interaction::{
        make_interaction_id, request_interaction_async, request_interaction_stdin,
        response_as_choice, response_as_text, InteractionRequest,
    };

    if points.is_empty() {
        return run_autonomous_stage(
            engine,
            entity,
            provider_name,
            model_name,
            max_iterations,
            tools,
            routing,
            compaction_config,
            executor,
        )
        .await;
    }

    let (run_id_owned, mut meta_holder): (Option<String>, Option<&mut RunMeta>) = match run_context
    {
        Some((rid, m)) => (Some(rid.to_string()), Some(m)),
        None => (None, None),
    };

    let segments = points.len() + 1;
    let iterations_per_segment = max_iterations / segments;
    let mut remaining_iterations = max_iterations;

    for (pt_idx, point) in points.iter().enumerate() {
        let iters = iterations_per_segment.min(remaining_iterations);
        if iters > 0 {
            let response = engine
                .run_inference_loop_filtered(
                    entity,
                    provider_name,
                    model_name,
                    tools.to_vec(),
                    iters,
                    None,
                    routing,
                    compaction_config,
                    executor,
                )
                .await;

            if let Ok(resp) = response {
                if !resp.content.is_empty() {
                    println!("{}", resp.content);
                    // Route agent response to the per-stage output file so the dashboard can display it
                    if let (Some(run_id), Some(ref m)) = (&run_id_owned, &meta_holder) {
                        record_stage_output(run_id, m.stage_index, &resp.content);
                    }
                }
                // Update token counts in meta so the dashboard shows them before WaitingInput
                if let Some(ref mut m) = meta_holder {
                    let token_line = format!(
                        "[Tokens: {} in, {} out]",
                        resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
                    );
                    record_stage_log(&m.run_id, m.stage_index, &token_line);
                    m.prompt_tokens += resp.tokens_used.prompt_tokens;
                    m.completion_tokens += resp.tokens_used.completion_tokens;
                    m.touch();
                    let _ = runstate::write_meta(m);
                }
            }
            remaining_iterations = remaining_iterations.saturating_sub(iters);
        }

        // Build the interaction request with the right style / options
        let req_id = make_interaction_id(pt_idx, 0);
        let bp_style = &point.style;
        let ipc_req = match bp_style {
            leviath_core::blueprint::InteractionStyle::MultipleChoice => {
                InteractionRequest::multiple_choice(
                    req_id,
                    &point.prompt,
                    point.options.clone(),
                    &point.name,
                )
            }
            leviath_core::blueprint::InteractionStyle::Confirm => {
                InteractionRequest::confirm(req_id, &point.prompt, &point.name)
            }
            leviath_core::blueprint::InteractionStyle::FreeText => {
                InteractionRequest::free_text(req_id, &point.prompt, &point.name, point.required)
            }
        };

        // Dispatch via file IPC or stdin
        let user_text =
            if let (Some(run_id), Some(ref mut meta)) = (&run_id_owned, &mut meta_holder) {
                let resp = request_interaction_async(run_id, meta, ipc_req.clone(), None).await?;
                match bp_style {
                    leviath_core::blueprint::InteractionStyle::MultipleChoice
                    | leviath_core::blueprint::InteractionStyle::Confirm => {
                        // Resolve choice index → option string
                        response_as_choice(&resp, &ipc_req.options)
                            .cloned()
                            .unwrap_or_else(|| response_as_text(&resp))
                    }
                    leviath_core::blueprint::InteractionStyle::FreeText => response_as_text(&resp),
                }
            } else {
                // Foreground (stdin) path — `request_interaction_stdin` prints and reads
                let resp = request_interaction_stdin(&ipc_req);
                match bp_style {
                    leviath_core::blueprint::InteractionStyle::MultipleChoice
                    | leviath_core::blueprint::InteractionStyle::Confirm => {
                        response_as_choice(&resp, &ipc_req.options)
                            .cloned()
                            .unwrap_or_else(|| response_as_text(&resp))
                    }
                    leviath_core::blueprint::InteractionStyle::FreeText => response_as_text(&resp),
                }
            };

        if !user_text.is_empty() {
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = user_text.len() / 4 + 1;
                let content = format!("User [{}]: {}", point.name, user_text);
                let _ = window.add_to_region("conversation", content, tokens);
            }
        }
    }

    if remaining_iterations > 0 {
        let response = engine
            .run_inference_loop_filtered(
                entity,
                provider_name,
                model_name,
                tools.to_vec(),
                remaining_iterations,
                None,
                routing,
                compaction_config,
                executor,
            )
            .await;

        if let Ok(resp) = response {
            if !resp.content.is_empty() {
                println!("{}", resp.content);
                if let (Some(run_id), Some(ref m)) = (&run_id_owned, &meta_holder) {
                    record_stage_output(run_id, m.stage_index, &resp.content);
                }
            }
            let token_line = format!(
                "[Tokens used: {} input, {} output]",
                resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
            );
            println!("\n{}", token_line);
            if let (Some(_), Some(ref m)) = (&run_id_owned, &meta_holder) {
                record_stage_log(&m.run_id, m.stage_index, &token_line);
            }
        }
    }

    Ok(())
}

// ─── Streaming inference ─────────────────────────────────────────────────────

/// Stream inference output directly to stdout, collecting the full response.
/// Used only for tool-less interactive stages.
async fn stream_inference(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    tool_filter: Option<&[String]>,
) -> anyhow::Result<leviath_providers::InferenceResponse> {
    let provider = engine
        .get_provider(provider_name)
        .ok_or_else(|| anyhow::anyhow!("Provider '{}' not registered", provider_name))?;

    let (messages, max_tokens) = {
        let window = engine
            .world()
            .get::<ContextWindow>(entity)
            .ok_or_else(|| anyhow::anyhow!("Entity has no ContextWindow"))?;

        let messages = window.assemble_messages();
        let remaining = window.max_tokens.saturating_sub(window.current_tokens);
        let max_tokens = remaining.min(4096);
        (messages, max_tokens)
    };

    // Tool-less streaming: always empty tools list
    let tools: Vec<leviath_providers::Tool> = Vec::new();
    let filtered_tools = if let Some(filter) = tool_filter {
        if filter.is_empty() {
            tools
        } else {
            tools
                .into_iter()
                .filter(|t| filter.iter().any(|f| f == &t.name))
                .collect()
        }
    } else {
        tools
    };

    // Respect each model's temperature support (e.g. claude-opus-4-8 deprecates it).
    let temperature = if provider.capabilities(model_name).supports_temperature {
        0.7
    } else {
        0.0
    };
    let request = leviath_providers::InferenceRequest {
        messages,
        model: model_name.to_string(),
        max_tokens,
        temperature,
        tools: filtered_tools,
        extra: serde_json::Value::Null,
    };

    let mut stream = provider
        .infer_stream(request)
        .await
        .map_err(|e| anyhow::anyhow!("Stream error: {}", e))?;

    let mut full_content = String::new();
    let mut final_tokens = None;
    let mut final_finish_reason = None;
    let mut all_tool_calls: Vec<leviath_providers::ToolCall> = Vec::new();

    print!("\nAssistant: ");
    use std::io::Write;

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| anyhow::anyhow!("Stream chunk error: {}", e))?;

        if !chunk.delta.is_empty() {
            print!("{}", chunk.delta);
            std::io::stdout().flush().ok();
            full_content.push_str(&chunk.delta);
        }

        if let Some(tokens) = chunk.tokens {
            final_tokens = Some(tokens);
        }
        if let Some(reason) = chunk.finish_reason {
            final_finish_reason = Some(reason);
        }

        for tc_delta in &chunk.tool_calls {
            while all_tool_calls.len() <= tc_delta.index {
                all_tool_calls.push(leviath_providers::ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: serde_json::Value::Null,
                });
            }
            let tc = &mut all_tool_calls[tc_delta.index];
            if let Some(ref id) = tc_delta.id {
                tc.id.clone_from(id);
            }
            if let Some(ref name) = tc_delta.name {
                tc.name.clone_from(name);
            }
            if !tc_delta.arguments_delta.is_empty() && tc.arguments.is_null() {
                if let Ok(val) = serde_json::from_str(&tc_delta.arguments_delta) {
                    tc.arguments = val;
                }
            }
        }
    }

    println!();

    if let Some(mut state) = engine
        .world_mut()
        .get_mut::<leviath_runtime::AgentState>(entity)
    {
        state.iteration += 1;
    }

    let tokens_used = final_tokens.unwrap_or(leviath_providers::TokenUsage {
        prompt_tokens: 0,
        completion_tokens: full_content.len() / 4,
        total_tokens: full_content.len() / 4,
    });

    Ok(leviath_providers::InferenceResponse {
        content: full_content,
        tool_calls: all_tool_calls,
        tokens_used,
        finish_reason: final_finish_reason.unwrap_or(leviath_providers::FinishReason::Complete),
    })
}

// ─── Shared helpers ───────────────────────────────────────────────────────────

/// Return the cheapest fast model for a given provider, used for title generation.
fn default_title_model(provider: &str) -> &'static str {
    match provider {
        "anthropic" | "claude-code" => "claude-haiku-4-5-20251001",
        "openai" => "gpt-5.4-mini",
        "google" => "gemini-3.5-flash",
        "openrouter" => "anthropic/claude-haiku-4-5",
        // For Ollama and unknown providers, fall through to the caller's
        // logic which will prefer config.default_model or the run model.
        _ => "",
    }
}

/// Attempt to generate a short title from the task prompt using a cheap model.
///
/// Best-effort: any failure is logged and silently ignored — a missing title
/// must never prevent the run from starting.  Token usage from this call is
/// intentionally excluded from the run's prompt/completion accumulators.
async fn generate_title(
    task: &str,
    config: &Config,
    registry: &leviath_runtime::ProviderRegistry,
    fallback_model: Option<&str>,
) -> Option<String> {
    let provider_name = config
        .title
        .provider
        .as_deref()
        .unwrap_or(&config.default_provider);

    let provider = registry.get(provider_name)?;

    let model = config
        .title
        .model
        .as_deref()
        .map(|s| s.to_string())
        .or_else(|| {
            let m = default_title_model(provider_name);
            if m.is_empty() {
                fallback_model.map(|s| s.to_string())
            } else {
                Some(m.to_string())
            }
        })?;

    let request = leviath_providers::InferenceRequest {
        messages: vec![
            leviath_providers::Message {
                role: "system".to_string(),
                content: "Write a terse 3-6 word title summarising the task. \
                          No quotes, no punctuation at the end, no markdown."
                    .to_string(),
            },
            leviath_providers::Message {
                role: "user".to_string(),
                content: task.to_string(),
            },
        ],
        model,
        max_tokens: 20,
        temperature: 0.0,
        tools: vec![],
        extra: serde_json::Value::Null,
    };

    match provider.infer(request).await {
        Ok(resp) => {
            let raw = resp.content.trim().lines().next()?.trim().to_string();
            // Strip leading # heading markers, backtick code formatting, surrounding quotes
            let title = raw
                .trim_start_matches('#')
                .trim()
                .trim_start_matches('`')
                .trim_end_matches('`')
                .trim_start_matches('"')
                .trim_end_matches('"')
                .trim_start_matches('\'')
                .trim_end_matches('\'')
                .trim()
                .to_string();
            if title.is_empty() {
                None
            } else {
                Some(title)
            }
        }
        Err(e) => {
            println!("Warning: title generation failed ({})", e);
            None
        }
    }
}

/// Build a ProviderRegistry from Config.
pub fn build_provider_registry(config: &Config) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();

    if let Some(ref key) = config.providers.anthropic_api_key {
        registry.register(
            "anthropic".to_string(),
            Arc::new(leviath_providers::AnthropicProvider::with_overrides(
                key.clone(),
                config.model_capabilities.clone(),
            )),
        );
    }

    if let Some(ref key) = config.providers.openai_api_key {
        registry.register(
            "openai".to_string(),
            Arc::new(leviath_providers::OpenAIProvider::with_overrides(
                key.clone(),
                config.model_capabilities.clone(),
            )),
        );
    }

    if let Some(ref key) = config.providers.google_api_key {
        registry.register(
            "google".to_string(),
            Arc::new(leviath_providers::GeminiProvider::with_overrides(
                key.clone(),
                config.model_capabilities.clone(),
            )),
        );
    }

    if let Some(ref key) = config.openrouter_api_key {
        registry.register(
            "openrouter".to_string(),
            Arc::new(leviath_providers::OpenRouterProvider::with_overrides(
                key.clone(),
                config.model_capabilities.clone(),
            )),
        );
    }

    let ollama_url = config
        .ollama_base_url
        .as_deref()
        .unwrap_or("http://localhost:11434");
    registry.register(
        "ollama".to_string(),
        Arc::new(leviath_providers::OllamaProvider::with_overrides(
            ollama_url.to_string(),
            config.model_capabilities.clone(),
        )),
    );

    // Claude Code provider (no API key needed - uses claude CLI subscription)
    registry.register(
        "claude-code".to_string(),
        Arc::new(leviath_providers::ClaudeCodeProvider::new()),
    );

    registry
}

/// Initialize context window regions on an entity from the blueprint.
pub fn initialize_context_window(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    blueprint: &Blueprint,
    task: &str,
) {
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        for region_def in &blueprint.context_layout.regions {
            let region = Region::new(
                region_def.name.clone(),
                region_def.kind.clone(),
                region_def.max_tokens,
            );
            window.add_region(region);
        }

        if window.get_region("tool_results").is_none() {
            let tool_region = Region::new("tool_results".to_string(), RegionKind::Temporary, 5000);
            window.add_region(tool_region);
        }

        if window.get_region("conversation").is_none() {
            let conv_region = Region::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow { max_items: 50 },
                10000,
            );
            window.add_region(conv_region);
        }

        let system_region_name = blueprint
            .context_layout
            .regions
            .iter()
            .find(|r| matches!(r.kind, RegionKind::Pinned))
            .map(|r| r.name.clone());

        if let Some(region_name) = system_region_name {
            let task_tokens = task.len() / 4 + 1;
            let _ = window.add_to_region(&region_name, task.to_string(), task_tokens);
        }
    }
}

/// Swap context layout to a stage-specific layout (preserving existing content where possible).
fn swap_context_layout(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    layout: &ContextLayout,
) {
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        let mut new_regions = Vec::new();
        for region_def in &layout.regions {
            let mut new_region = Region::new(
                region_def.name.clone(),
                region_def.kind.clone(),
                region_def.max_tokens,
            );

            if let Some(existing) = window.get_region(&region_def.name) {
                for entry in &existing.content {
                    let _ = new_region.add_entry(entry.content.clone(), entry.tokens);
                }
            }

            new_regions.push(new_region);
        }

        window.regions = new_regions;
        window.current_tokens = window.calculate_tokens();
    }
}

fn find_manifest(path: &str) -> anyhow::Result<PathBuf> {
    let p = Path::new(path);

    // 1. Explicit agent.leviath file
    if p.is_file() && p.file_name() == Some(std::ffi::OsStr::new("agent.leviath")) {
        return Ok(p.to_path_buf());
    }

    // 2. Directory with agent.leviath inside
    if p.is_dir() {
        let manifest = p.join("agent.leviath");
        if manifest.exists() {
            return Ok(manifest);
        }
    }

    // 3. Installed agent by name: ~/.leviath/agents/<name>/agent.leviath
    if let Some(home) = dirs::home_dir() {
        let installed = home
            .join(".leviath")
            .join("agents")
            .join(path)
            .join("agent.leviath");
        if installed.exists() {
            return Ok(installed);
        }
    }

    // 4. agent.leviath in current directory (for `lev run` with no path)
    let current_manifest = PathBuf::from("agent.leviath");
    if current_manifest.exists() {
        return Ok(current_manifest);
    }

    anyhow::bail!(
        "Could not find agent manifest for '{}'. \
        Pass a path to a directory containing agent.leviath, \
        or an installed agent name (see `lev list`).",
        path
    )
}

/// Public alias for parse_manifest (used by dashboard).
pub fn parse_manifest_public(content: &str) -> anyhow::Result<Blueprint> {
    parse_manifest(content)
}

/// Parse an agent.leviath TOML manifest into a Blueprint.
fn parse_manifest(content: &str) -> anyhow::Result<Blueprint> {
    let parsed: toml::Value = toml::from_str(content)
        .map_err(|e| anyhow::anyhow!("Failed to parse agent.leviath: {}", e))?;

    let agent = parsed
        .get("agent")
        .ok_or_else(|| anyhow::anyhow!("Missing [agent] section"))?;

    let name = agent
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unnamed")
        .to_string();
    let version = agent
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.1.0")
        .to_string();
    let description = agent
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let max_child_depth = agent
        .get("max_child_depth")
        .and_then(|v| v.as_integer())
        .map(|v| v as usize);

    let entry_stage = agent
        .get("entry_stage")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut stages = Vec::new();
    if let Some(stages_table) = parsed.get("stages").and_then(|v| v.as_table()) {
        for (stage_name, stage_value) in stages_table {
            let model_table = stage_value.get("model").and_then(|v| v.as_table());
            let model_config = if let Some(mt) = model_table {
                ModelConfig::new(
                    mt.get("provider")
                        .and_then(|v| v.as_str())
                        .unwrap_or("anthropic")
                        .to_string(),
                    mt.get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("claude-sonnet-4-6")
                        .to_string(),
                )
            } else {
                ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string())
            };

            let mut stage = Stage::new(stage_name.clone(), model_config);

            if let Some(mode_str) = stage_value.get("mode").and_then(|v| v.as_str()) {
                stage = match mode_str {
                    "interactive" => stage.with_mode(StageMode::Interactive),
                    "interactive_points" => {
                        let mut points = Vec::new();
                        if let Some(pts_arr) = stage_value
                            .get("interaction_points")
                            .and_then(|v| v.as_array())
                        {
                            for pt in pts_arr {
                                let pt_name = pt
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let pt_prompt = pt
                                    .get("prompt")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let pt_required =
                                    pt.get("required").and_then(|v| v.as_bool()).unwrap_or(true);
                                let pt_style = match pt.get("style").and_then(|v| v.as_str()) {
                                    Some("multiple_choice") => {
                                        leviath_core::blueprint::InteractionStyle::MultipleChoice
                                    }
                                    Some("confirm") => {
                                        leviath_core::blueprint::InteractionStyle::Confirm
                                    }
                                    _ => leviath_core::blueprint::InteractionStyle::FreeText,
                                };
                                // Accept either "options" or "choices" key
                                let pt_options: Vec<String> = pt
                                    .get("options")
                                    .or_else(|| pt.get("choices"))
                                    .and_then(|v| v.as_array())
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                points.push(leviath_core::blueprint::InteractionPoint {
                                    name: pt_name,
                                    prompt: pt_prompt,
                                    required: pt_required,
                                    style: pt_style,
                                    options: pt_options,
                                });
                            }
                        }
                        stage.with_mode(StageMode::InteractivePoints { points })
                    }
                    _ => stage.with_mode(StageMode::Autonomous),
                };
            }

            if let Some(max_iter) = stage_value
                .get("max_iterations")
                .and_then(|v| v.as_integer())
            {
                stage.max_iterations = Some(max_iter as usize);
            }

            if let Some(tools_arr) = stage_value
                .get("available_tools")
                .and_then(|v| v.as_array())
            {
                stage.available_tools = tools_arr
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
            }

            if let Some(sp) = stage_value.get("system_prompt").and_then(|v| v.as_str()) {
                stage.config.insert(
                    "system_prompt".to_string(),
                    serde_json::Value::String(sp.trim().to_string()),
                );
            }

            if let Some(routing_table) = stage_value.get("tool_routing").and_then(|v| v.as_table())
            {
                let mut routing = ToolResultRouting::default();

                if let Some(dr) = routing_table.get("default_region").and_then(|v| v.as_str()) {
                    routing.default_region = dr.to_string();
                }
                if let Some(p) = routing_table.get("persist").and_then(|v| v.as_bool()) {
                    routing.persist = p;
                }
                if let Some(mt) = routing_table
                    .get("max_result_tokens")
                    .and_then(|v| v.as_integer())
                {
                    routing.max_result_tokens = Some(mt as usize);
                }
                if let Some(overrides_table) =
                    routing_table.get("overrides").and_then(|v| v.as_table())
                {
                    for (tool_name, region_val) in overrides_table {
                        if let Some(region_name) = region_val.as_str() {
                            routing
                                .tool_overrides
                                .insert(tool_name.clone(), region_name.to_string());
                        }
                    }
                }

                stage.tool_result_routing = Some(routing);
            }

            // Parse requires_children flag
            if let Some(rc) = stage_value
                .get("requires_children")
                .and_then(|v| v.as_bool())
            {
                stage.requires_children = rc;
            }

            // Parse per-stage tool permissions: [stages.<name>.tool_permissions]
            if let Some(tp_table) = stage_value
                .get("tool_permissions")
                .and_then(|v| v.as_table())
            {
                for (tool_name, policy_val) in tp_table {
                    if let Some(policy_str) = policy_val.as_str() {
                        stage
                            .tool_permissions
                            .insert(tool_name.clone(), policy_str.to_string());
                    }
                }
            }

            // Parse max_revisits
            if let Some(mr) = stage_value.get("max_revisits").and_then(|v| v.as_integer()) {
                stage.max_revisits = Some(mr as usize);
            }

            // Parse transition_prompt
            if let Some(tp) = stage_value
                .get("transition_prompt")
                .and_then(|v| v.as_str())
            {
                stage.transition_prompt = Some(tp.trim().to_string());
            }

            // Parse transitions: [stages.<name>.transitions.<target>]
            if let Some(transitions_table) =
                stage_value.get("transitions").and_then(|v| v.as_table())
            {
                let mut transitions = std::collections::HashMap::new();
                for (target_name, edge_value) in transitions_table {
                    let hint = edge_value
                        .get("hint")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    let condition = match edge_value.get("condition").and_then(|v| v.as_str()) {
                        Some("error") => leviath_core::blueprint::TransitionCondition::Error,
                        Some("max_iterations") => {
                            leviath_core::blueprint::TransitionCondition::MaxIterations
                        }
                        Some("llm_choice") => {
                            leviath_core::blueprint::TransitionCondition::LlmChoice
                        }
                        Some("always") | None => {
                            leviath_core::blueprint::TransitionCondition::Always
                        }
                        Some(custom) => {
                            leviath_core::blueprint::TransitionCondition::Custom(custom.to_string())
                        }
                    };

                    let transform = match edge_value.get("transform").and_then(|v| v.as_str()) {
                        Some("clear") => leviath_core::blueprint::EdgeTransform::Clear,
                        Some("compact") | Some("summarize") => {
                            leviath_core::blueprint::EdgeTransform::Compact { prompt: None }
                        }
                        Some("custom") => {
                            // Parse transform_config sub-table
                            let tc = edge_value.get("transform_config");
                            let carry = tc
                                .and_then(|v| v.get("carry"))
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let compact = tc
                                .and_then(|v| v.get("compact"))
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let clear = tc
                                .and_then(|v| v.get("clear"))
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let compact_prompt = tc
                                .and_then(|v| v.get("compact_prompt"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            leviath_core::blueprint::EdgeTransform::Custom {
                                carry,
                                compact,
                                clear,
                                compact_prompt,
                            }
                        }
                        Some("direct") | None => leviath_core::blueprint::EdgeTransform::Direct,
                        Some(_) => leviath_core::blueprint::EdgeTransform::Direct,
                    };

                    transitions.insert(
                        target_name.clone(),
                        leviath_core::blueprint::TransitionEdge {
                            target: target_name.clone(),
                            condition,
                            hint,
                            transform,
                        },
                    );
                }
                stage.transitions = Some(transitions);
            }

            stages.push(stage);
        }
    }

    if stages.is_empty() {
        stages.push(Stage::new(
            "main".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        ));
    }

    let mut regions = Vec::new();
    let mut total_tokens = 0usize;

    if let Some(regions_table) = parsed
        .get("context")
        .and_then(|v| v.get("regions"))
        .and_then(|v| v.as_table())
    {
        for (region_name, region_value) in regions_table {
            let max_tokens = region_value
                .get("max_tokens")
                .and_then(|v| v.as_integer())
                .unwrap_or(5000) as usize;

            let kind_str = region_value
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("temporary");

            let kind = match kind_str {
                "pinned" => RegionKind::Pinned,
                "sliding_window" => {
                    let max_items = region_value
                        .get("max_items")
                        .and_then(|v| v.as_integer())
                        .unwrap_or(10) as usize;
                    RegionKind::SlidingWindow { max_items }
                }
                "temporary" => RegionKind::Temporary,
                "compacting" => {
                    let threshold = region_value
                        .get("threshold_tokens")
                        .and_then(|v| v.as_integer())
                        .unwrap_or((max_tokens as i64) * 8 / 10)
                        as usize;
                    RegionKind::Compacting {
                        threshold_tokens: threshold,
                    }
                }
                "clearable" => RegionKind::Clearable,
                "compact_history" => {
                    let source = region_value
                        .get("source_region")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    RegionKind::CompactHistory {
                        source_region: source,
                    }
                }
                _ => RegionKind::Temporary,
            };

            total_tokens += max_tokens;
            regions.push(RegionDefinition::new(region_name.clone(), kind, max_tokens));
        }
    }

    if regions.is_empty() {
        regions.push(RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            2000,
        ));
        regions.push(RegionDefinition::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow { max_items: 10 },
            10000,
        ));
        total_tokens = 12000;
    }

    let layout = ContextLayout::new(regions, total_tokens);

    let mut blueprint = Blueprint::new(name, description, stages, layout);
    blueprint.version = version;
    blueprint.max_child_depth = max_child_depth;
    blueprint.entry_stage = entry_stage;

    if let Some(compaction_table) = parsed.get("compaction").and_then(|v| v.as_table()) {
        let mut cc = CompactionConfig::default();

        if let Some(provider) = compaction_table.get("provider").and_then(|v| v.as_str()) {
            cc.provider = provider.to_string();
        }
        if let Some(model) = compaction_table.get("model").and_then(|v| v.as_str()) {
            cc.model = model.to_string();
        }
        if let Some(sp) = compaction_table
            .get("system_prompt")
            .and_then(|v| v.as_str())
        {
            cc.system_prompt = Some(sp.to_string());
        }
        if let Some(mst) = compaction_table
            .get("max_summary_tokens")
            .and_then(|v| v.as_integer())
        {
            cc.max_summary_tokens = mst as usize;
        }
        if let Some(temp) = compaction_table
            .get("temperature")
            .and_then(|v| v.as_float())
        {
            cc.temperature = temp as f32;
        }

        blueprint.compaction_config = Some(cc);
    }

    // Parse agent-level tool permissions: [tool_permissions]
    if let Some(tp_table) = parsed.get("tool_permissions").and_then(|v| v.as_table()) {
        for (tool_name, policy_val) in tp_table {
            if let Some(policy_str) = policy_val.as_str() {
                blueprint.metadata.insert(
                    format!("tool_perm:{}", tool_name),
                    serde_json::Value::String(policy_str.to_string()),
                );
            }
        }
    }

    Ok(blueprint)
}

/// Snapshot the current context window to `context.json` for the background dashboard.
/// No-op when running in foreground mode (run_id is None).
fn write_context_snapshot_if_bg(
    engine: &AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    stage_name: &str,
    run_id: &Option<String>,
) {
    let Some(ref rid) = run_id else { return };
    let Some(snap) = build_context_snapshot(engine, entity, stage_name) else {
        return;
    };
    let _ = runstate::write_context_snapshot(rid, &snap);
}

/// Build a ContextSnapshot from the current engine state (reused by legacy and per-stage writes).
fn build_context_snapshot(
    engine: &AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    stage_name: &str,
) -> Option<runstate::ContextSnapshot> {
    let window = engine.world().get::<ContextWindow>(entity)?;
    use leviath_core::RegionKind;
    let regions = window
        .regions
        .iter()
        .map(|r| {
            let entries = r
                .content
                .iter()
                .map(|e| runstate::RegionEntrySnapshot {
                    content: e.content.clone(),
                    tokens: e.tokens,
                    metadata: e.metadata.clone(),
                })
                .collect();
            runstate::RegionSnapshot {
                name: r.name.clone(),
                kind: match &r.kind {
                    RegionKind::Pinned => "pinned",
                    RegionKind::Temporary => "temporary",
                    RegionKind::Clearable => "clearable",
                    RegionKind::SlidingWindow { .. } => "sliding",
                    RegionKind::Compacting { .. } => "compacting",
                    RegionKind::CompactHistory { .. } => "history",
                }
                .to_string(),
                current_tokens: r.current_tokens,
                max_tokens: r.max_tokens,
                entries,
            }
        })
        .collect();
    Some(runstate::ContextSnapshot {
        stage_name: stage_name.to_string(),
        total_tokens: window.current_tokens,
        max_tokens: window.max_tokens,
        regions,
    })
}
