//! The tools `lev mcp serve` offers, and what each does.
//!
//! Every tool is a thin bridge: a `ControlRequest` to the daemon, a read of a
//! run's record on disk, or one call into a library core the CLI already has
//! (`resolve_spawn_args`, `install_script_tool`, `ScriptToolSet::discover`).
//! Every result is a text block plus the same answer as `structuredContent`.
//! A tool that ran and failed answers `isError: true` text the model can act
//! on; a call the server could not attempt (unknown tool, a malformed
//! argument, a relative workdir) is a JSON-RPC `-32602`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use leviath_core::interaction::{ApprovalScope, InteractionResponse};
use leviath_core::output::OutputSpec;
use leviath_core::run_meta::RunStatus;
use leviath_core::text::{floor_char_boundary, substring, truncate_at_boundary};
use leviath_mcp::server::{ServerTool, ToolAnnotations, text_result};
use leviath_runtime::control_socket::{ControlRequest, ControlResponse, WorldEventStream};
use leviath_runtime::host::{SpawnArgs, WorldEvent};
use leviath_runtime::persistence::run_status_for_label;
use serde_json::{Map, Value, json};

use super::serve::{MCP_TEXT_CAP, Progress, Shared};
use crate::daemon::client::{
    LaunchRequest, held_checkpoint_warning_for_spawn, never_interactive,
    read_path_warning_for_spawn, resolve_spawn_args, spawn_once,
};
use crate::daemon::wait::{WaitOutcome, wait_for_run_with};
use crate::runstate::{
    force_cancel_in, is_terminal_status, list_runs_in, read_final_output_in, read_meta_from,
    run_dir_in,
};
use crate::workdir_guard::{WorkdirVerdict, assess};

/// How a tool call ends.
pub(super) enum CallOutcome {
    /// A `tools/call` result, `isError` included.
    Result(Value),
    /// The call could not be attempted: a JSON-RPC `-32602`.
    InvalidParams(String),
}

/// The arguments of one call, already known to be an object.
type Args = Map<String, Value>;

/// The `run` tool's description; it carries the two sentences every host
/// model has to read before it reaches for its own subagent tool or gives up
/// on a slow run.
const RUN_DESCRIPTION: &str = "Delegate a task to the Leviath agent runtime and wait for its \
    final output. Use this instead of spawning a subagent when the user says leviath. Pass an \
    absolute `workdir` (the project directory). The default agent runs multi-stage work with \
    verification; `list_agents` shows the others. A host timeout or cancellation only stops \
    waiting; the run continues (list_runs, wait, status, cancel). With `wait: false` the call \
    returns the run_id at once for `wait`. A result with status waiting_input needs `respond` \
    with its request_id, then `wait`.";

/// The `wait` tool's description.
const WAIT_DESCRIPTION: &str = "Wait for an existing Leviath run to finish and return its \
    final output. Use after `run` with `wait: false`, or after a host timeout interrupted a `run`: \
    a host timeout or cancellation only stops waiting; the run continues (list_runs, wait, \
    status, cancel).";

/// The tool table, in the order hosts list it.
pub(super) fn tool_table() -> Vec<ServerTool> {
    let tool =
        |name: &str, title: &str, description: &str, schema: Value, annotations| ServerTool {
            name: name.to_string(),
            title: title.to_string(),
            description: description.to_string(),
            input_schema: schema,
            annotations,
        };
    let string = |description: &str| json!({ "type": "string", "description": description });
    let boolean = |description: &str| json!({ "type": "boolean", "description": description });
    let integer =
        |description: &str| json!({ "type": "integer", "minimum": 0, "description": description });
    let object = |properties: Value, required: &[&str]| json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false });
    let run_id = || string("The run id, as returned by `run` or listed by `list_runs`.");
    vec![
        tool(
            "run",
            "Run a Leviath agent",
            RUN_DESCRIPTION,
            object(
                json!({
                    "task": string("The task, self-contained: goal, constraints, and what done looks like."),
                    "agent": string("The agent: an installed name (orchestrator, coder, deep-researcher, ...), a directory, or an agent.leviath path. Default: the server's default agent."),
                    "workdir": string("Absolute path of the project directory the agent works in. Strongly recommended; a relative path is refused."),
                    "wait": boolean("Wait for the run and return its final output (default true). false returns the run_id at once."),
                    "timeout_secs": integer("Stop waiting after this many seconds and return the run's status; 0 (default) waits until it finishes. Never cancels the run."),
                    "yolo": boolean("Run unattended: approve every tool call and answer the agent's own prompts. Default: true unless the server was started with --attended. Required explicitly for an agent whose [read_paths] are granted."),
                    "model": string("Model override, `provider/model` or a bare model name."),
                    "allow": { "type": "array", "items": { "type": "string" }, "description": "Tools to allow outright." },
                    "max_depth": integer("Cap on the sub-agent tree depth."),
                    "no_seed_commands": boolean("Refuse the blueprint's command seeds (shell commands run at spawn)."),
                    "regions": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Caller-input regions by name, for agents that take them (a reviewer's `diff`, say)." },
                    "output_format": string("Ask for the final output in this shape (markdown, json, a house format)."),
                    "output_instructions": string("Extra guidance about that shape."),
                    "output_schema": { "type": "object", "description": "A JSON Schema the final output must satisfy." },
                }),
                &["task"],
            ),
            ToolAnnotations::OPEN_WORLD,
        ),
        tool(
            "wait",
            "Wait for a run",
            WAIT_DESCRIPTION,
            object(
                json!({
                    "run_id": run_id(),
                    "timeout_secs": integer("Stop waiting after this many seconds; 0 (default) waits until the run finishes. Never cancels the run."),
                }),
                &["run_id"],
            ),
            ToolAnnotations::READ_ONLY,
        ),
        tool(
            "status",
            "Run status",
            "Where a Leviath run stands: status, stage, iteration, token counts, and whether it has a final output. Reads the run's record; asks the daemon only for a run still going.",
            object(json!({ "run_id": run_id() }), &["run_id"]),
            ToolAnnotations::READ_ONLY,
        ),
        tool(
            "result",
            "Run result",
            "A finished Leviath run's final output, paged by byte offset when it is long. Needs no daemon.",
            object(
                json!({
                    "run_id": run_id(),
                    "offset": integer("Byte offset to start from (default 0); cut to a character boundary."),
                    "max_bytes": integer("At most this many bytes (default 49152)."),
                }),
                &["run_id"],
            ),
            ToolAnnotations::READ_ONLY,
        ),
        tool(
            "cancel",
            "Cancel a run",
            "Stop a Leviath run. The explicit way to end one: a host timeout never does this.",
            object(json!({ "run_id": run_id() }), &["run_id"]),
            ToolAnnotations::DESTRUCTIVE,
        ),
        tool(
            "message",
            "Message a run",
            "Send a message to a running Leviath agent whose current stage accepts messages.",
            object(
                json!({
                    "run_id": run_id(),
                    "content": string("The message."),
                    "target_region": string("The context region to deliver into, when the stage names several."),
                }),
                &["run_id", "content"],
            ),
            ToolAnnotations::MUTATING,
        ),
        tool(
            "respond",
            "Answer a run's question",
            "Answer an interaction a Leviath run is waiting on (a tool approval, a question, a plan review). Without `request_id`, lists every open interaction. Then call `wait`.",
            object(
                json!({
                    "request_id": string("The interaction's id, from a waiting_input result or the listing."),
                    "value": string("Free-text answer."),
                    "choice_index": integer("Chosen option, for a multiple-choice question."),
                    "approved": boolean("Approve (true) or deny (false) a tool call or plan."),
                    "scope": { "type": "string", "enum": ["once", "stage", "session"], "description": "How far an approval reaches (default once)." },
                    "feedback": string("With approved=false: what the agent should do instead."),
                }),
                &[],
            ),
            ToolAnnotations::MUTATING,
        ),
        tool(
            "list_runs",
            "List runs",
            "Leviath runs, newest first: the daemon's live and recently finished runs plus finished runs on disk. Use it to find a run_id after a host timeout interrupted `run`.",
            object(
                json!({
                    "limit": integer("At most this many runs (default 20)."),
                    "include_finished_on_disk": boolean("Include runs the daemon no longer holds (default true)."),
                }),
                &[],
            ),
            ToolAnnotations::READ_ONLY,
        ),
        tool(
            "list_agents",
            "List agents",
            "The Leviath agents available to `run`: installed blueprints (name, version, description, whether they take a task or named inputs) and bundled ones `run` installs on demand.",
            object(json!({}), &[]),
            ToolAnnotations::READ_ONLY,
        ),
        tool(
            "install_tool",
            "Install a Rhai tool",
            "Compile a Rhai tool script and install it into Leviath's global tools directory, where every future agent run can call it. Refuses a script that does not compile, lacks `// @tool <name>` or `// @description`, or collides with an existing tool name. Use for repeatable mechanical steps, never for judgement; call `list_tools` first.",
            object(
                json!({
                    "name": string("The tool name; must match the script's `// @tool` directive. `<domain>_<verb>`, e.g. cargo_lint_all."),
                    "source": string("The Rhai source, with `// @tool`, `// @description`, `// @param` and `// @requires` annotations."),
                    "overwrite": boolean("Replace an existing script of the same name (default false)."),
                }),
                &["name", "source"],
            ),
            ToolAnnotations::OPEN_WORLD,
        ),
        tool(
            "list_tools",
            "List Rhai tools",
            "The global Rhai tools every Leviath agent can be offered: name, description, parameters, requirements, source file and who installed it, plus files that failed to compile.",
            object(json!({}), &[]),
            ToolAnnotations::READ_ONLY,
        ),
    ]
}

/// Run one tool.
///
/// The arguments are held to the tool's advertised schema first, by the same
/// validator the runtime applies to an agent's tool calls, so a wrong type, a
/// missing required key or an unknown key is one `-32602` naming the
/// violation, and the handlers read their arguments without a failure path
/// each. A schema that would not compile is skipped by that validator; a test
/// holds every schema in the table to compiling.
pub(super) async fn call_tool(
    shared: &Shared,
    name: &str,
    arguments: Value,
    progress: &Progress,
    run_slot: &Arc<Mutex<Option<String>>>,
) -> CallOutcome {
    let Some(tool) = shared.tools.iter().find(|t| t.name == name) else {
        return CallOutcome::InvalidParams(format!("unknown tool '{name}'"));
    };
    if let leviath_tools::validate::ArgValidation::Invalid(message) =
        leviath_tools::validate::validate_tool_args(name, &tool.input_schema, &arguments)
    {
        return CallOutcome::InvalidParams(message);
    }
    // `parse_call` only lets an object through; the fallback is for the type.
    let args = arguments.as_object().cloned().unwrap_or_default();
    match name {
        "run" => run::run(shared, &args, progress, run_slot).await,
        "wait" => run::wait(shared, &args, progress, run_slot).await,
        "status" => inspect::status(shared, &args).await,
        "result" => inspect::result(shared, &args),
        "cancel" => control::cancel(shared, &args).await,
        "message" => control::message(shared, &args).await,
        "respond" => control::respond(shared, &args).await,
        "list_runs" => inspect::list_runs(shared, &args).await,
        "list_agents" => inspect::list_agents(shared),
        "install_tool" => scripts::install_tool(shared, &args),
        // The table and this match are held together by a test; the
        // fallback is for the type, not for a tool.
        _ => scripts::list_tools(shared),
    }
}

// ─── Argument access ──────────────────────────────────────────────────────────
//
// `call_tool` has already held the arguments to the tool's schema, so these
// only read: a required key is present, an optional one is absent or of its
// declared type, and nothing here can fail.

fn str_arg(args: &Args, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn bool_arg(args: &Args, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn uint_arg(args: &Args, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn str_list_arg(args: &Args, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn str_map_arg(args: &Args, key: &str) -> HashMap<String, String> {
    args.get(key)
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn object_arg(args: &Args, key: &str) -> Option<Value> {
    args.get(key).filter(|v| v.is_object()).cloned()
}

// ─── Results ──────────────────────────────────────────────────────────────────

/// A successful result: the text, capped for the host, and its data.
fn ok(text: String, structured: Value, location: Option<&str>) -> CallOutcome {
    CallOutcome::Result(finish(text, false, structured, location))
}

/// A failed result: the tool ran, and this is why it did not do the job.
fn fail(text: String, structured: Value) -> CallOutcome {
    CallOutcome::Result(finish(text, true, structured, None))
}

/// Cap the text at [`MCP_TEXT_CAP`] and record the cut in the data.
/// `location` names where the full text lives, when it lives somewhere.
fn finish(text: String, is_error: bool, mut structured: Value, location: Option<&str>) -> Value {
    let (text, truncated) = cap_text(&text, location);
    if truncated && let Some(obj) = structured.as_object_mut() {
        obj.insert("host_truncated".to_string(), Value::Bool(true));
        if let Some(Value::Object(output)) = obj.get_mut("final_output") {
            output.insert("host_truncated".to_string(), Value::Bool(true));
        }
    }
    text_result(text, is_error, Some(structured))
}

/// Cut `text` at the cap on a character boundary, appending one line that
/// says so and where the rest is. Returns whether it cut.
pub(super) fn cap_text(text: &str, location: Option<&str>) -> (String, bool) {
    if text.len() <= MCP_TEXT_CAP {
        return (text.to_string(), false);
    }
    let kept = truncate_at_boundary(text, MCP_TEXT_CAP);
    let rest = match location {
        Some(at) => format!("; full text with `result` (offset/max_bytes) or at {at}"),
        None => String::new(),
    };
    (format!("{kept}\noutput truncated for the host{rest}"), true)
}

/// A daemon status label in the vocabulary every other surface speaks, with
/// a word this build does not know passed through rather than invented.
pub(super) fn wire_status(label: &str) -> String {
    run_status_for_label(label)
        .map(|s| s.wire().to_string())
        .unwrap_or_else(|| label.to_string())
}

/// Where a run's answer lives on disk, for the truncation note.
fn output_location(shared: &Shared, run_id: &str) -> String {
    run_dir_in(&shared.env.runs_dir, run_id)
        .join(leviath_core::FINAL_OUTPUT_FILE)
        .display()
        .to_string()
}

/// Send a request whose answer is a boolean: `Ok(applied)` on `ok: true`,
/// `Err` with `refused` on `ok: false`, and the daemon's own words otherwise.
async fn bool_request(
    shared: &Shared,
    request: ControlRequest,
    applied: &str,
    refused: &str,
) -> Result<String, String> {
    if let Err(e) = shared.daemon_ready().await {
        return Err(format!("the leviath daemon is not available: {e}"));
    }
    match shared.control.request(&request).await {
        Ok(ControlResponse::Ok { ok: true }) => Ok(applied.to_string()),
        Ok(ControlResponse::Ok { ok: false }) => Err(refused.to_string()),
        Ok(other) => Err(format!("unexpected daemon response: {other:?}")),
        Err(e) => Err(format!("the leviath daemon is not reachable ({e})")),
    }
}

// The tools themselves, by concern. Each takes the shared helpers above
// through `use super::*`.
mod control;
mod inspect;
mod run;
mod scripts;

// The pure pieces the server's tests exercise directly.
#[cfg(test)]
pub(crate) use control::build_interaction_response;
#[cfg(test)]
pub(crate) use run::{has_granted_read_paths, missing_bundled};
