//! The sub-agent tools that start nothing: `spawn_schema`,
//! `describe_blueprint`, `validate_spawn`'s answer, and `run_history`.
//!
//! Each answers with JSON (or, for a run's whole state, the TOML view `lev
//! run show` prints), so the model reads exactly the names it will write back.

use leviath_runtime::host::{RunHistory, SubAgentOp};
use leviath_runtime::spec::graph::StageMode;
use leviath_runtime::spec::names::BlueprintRef;
use leviath_runtime::spec::summary::SpawnSummary;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::SubAgentHandle;

/// JSON as the model reads it.
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("a JSON value always prints")
}

/// `spawn_schema`: the request's top level with the names of its parts, or
/// one part by name. The whole schema is some hundred kilobytes, far more
/// than one prompt should carry for the one part a model needs.
pub(super) fn spawn_schema(args: &Value) -> String {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        #[serde(default)]
        part: Option<String>,
    }
    let part = match serde_json::from_value::<Args>(args.clone()) {
        Ok(args) => args.part,
        Err(e) => return format!("[error] spawn_schema takes {{\"part\": \"<name>\"}}: {e}"),
    };
    let mut schema = leviath_runtime::runfile::spawn_request_schema();
    let defs = schema
        .as_object_mut()
        .and_then(|root| root.remove("$defs"))
        .and_then(|defs| defs.as_object().cloned())
        .unwrap_or_default();
    match part.filter(|p| !p.trim().is_empty()) {
        None => {
            let names: Vec<&String> = defs.keys().collect();
            format!(
                "The spawn request's top level. Each `$ref` names a part: ask for it with \
                 {{\"part\": \"<name>\"}}. spawn_agent's `source.graph` is a RunGraph, and its \
                 `inputs` are the RawInput values a graph's InputDecls check.\n{}\nParts: {}",
                pretty(&schema),
                names
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        Some(name) => match defs.get(&name) {
            Some(def) => format!(
                "The part {name}. Each `$ref` names another part to ask for.\n{}",
                pretty(def)
            ),
            None => format!(
                "[error] the spawn request has no part '{name}'. Parts: {}",
                defs.keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    }
}

/// A stage's mode, in a word.
pub(super) fn mode_label(mode: &StageMode) -> &'static str {
    match mode {
        StageMode::Autonomous => "autonomous",
        StageMode::Interactive => "interactive",
        StageMode::InteractivePoints(_) => "interactive_points",
        StageMode::FanOut(_) => "fan_out",
        StageMode::Output => "output",
    }
}

/// `describe_blueprint`: an installed blueprint's purpose, stages and
/// declared inputs, with a call that would spawn it.
pub(super) fn describe_blueprint(h: &SubAgentHandle, args: &Value) -> String {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        blueprint: Value,
    }
    let named = serde_json::from_value::<Args>(args.clone())
        .map_err(|e| e.to_string())
        .and_then(|args| match args.blueprint {
            Value::String(text) => BlueprintRef::parse(&text).map_err(|e| e.to_string()),
            other => serde_json::from_value::<BlueprintRef>(other).map_err(|e| e.to_string()),
        });
    let reference = match named {
        Ok(reference) => reference,
        Err(e) => {
            return format!(
                "[error] describe_blueprint takes {{\"blueprint\": \"<name>\"}} or \
                 {{\"blueprint\": {{\"name\": \"<name>\", \"digest\": \"<hex>\"}}}}: {e}"
            );
        }
    };
    let loaded =
        match crate::daemon::resolve_env::load_installed(h.agents_dir.as_deref(), &reference) {
            Ok(loaded) => loaded,
            Err(issue) => return format!("[error] {issue}"),
        };
    let graph = &loaded.graph;
    let example: serde_json::Map<String, Value> = graph
        .inputs
        .iter()
        .map(|d| (d.name.to_string(), json!(format!("<{}>", d.name))))
        .collect();
    pretty(&json!({
        "blueprint": loaded.reference.to_string(),
        "version": loaded.version,
        "title": graph.title,
        "description": graph.description,
        "entry_stage": graph.entry,
        "stages": graph.stages.iter().map(|s| json!({
            "name": s.name,
            "description": s.description,
            "mode": mode_label(&s.mode),
        })).collect::<Vec<_>>(),
        "inputs": graph.inputs,
        "spawn_with": {
            "source": {"blueprint": loaded.reference.name},
            "inputs": example,
        },
    }))
}

/// What `validate_spawn` says of a spawn that would start.
pub(super) fn valid(summary: &SpawnSummary) -> String {
    let warned = crate::commands::run::request::warnings_report(&summary.warnings);
    warned
        .into_iter()
        .chain(std::iter::once(format!(
            "Valid: spawn_agent with these arguments would start the run below. Nothing was \
             started.\n{}",
            pretty(&serde_json::to_value(summary).expect("a summary is plain data"))
        )))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `run_history`'s arguments.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryArgs {
    run_id: String,
    #[serde(default)]
    view: View,
    #[serde(default)]
    at: Option<u64>,
}

/// What `run_history` returns.
#[derive(Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(super) enum View {
    /// The run in brief.
    #[default]
    Summary,
    /// The whole state.
    State,
    /// Each edge the run took.
    Transitions,
}

/// `run_history`: read a run in the caller's own tree through the host.
pub(super) async fn run_history(h: &SubAgentHandle, args: &Value) -> String {
    let args = match serde_json::from_value::<HistoryArgs>(args.clone()) {
        Ok(args) => args,
        Err(e) => {
            return format!(
                "[error] run_history takes {{\"run_id\": \"<id>\", \"view\": \"summary\" | \
                 \"state\" | \"transitions\", \"at\": <step>}}: {e}"
            );
        }
    };
    let (tx, rx) = oneshot::channel();
    if h.sender
        .send(SubAgentOp::History {
            run_id: args.run_id.clone(),
            caller_run_id: h.parent_run_id.clone(),
            at: args.at,
            reply: tx,
        })
        .is_err()
    {
        return "[error] the daemon is shutting down".to_string();
    }
    match rx.await {
        Ok(Ok(history)) => render_history(&args.run_id, args.view, &history),
        Ok(Err(why)) => format!("[error] {why}"),
        Err(_) => "[error] the daemon dropped the history request".to_string(),
    }
}

/// A run's history, as `view` asks for it.
pub(super) fn render_history(run_id: &str, view: View, history: &RunHistory) -> String {
    let state = &history.state;
    let transitions: Vec<Value> = history
        .transitions
        .iter()
        .map(|(seq, t)| {
            json!({
                "step": seq,
                "from": t.from,
                "to": t.to,
                "edge": t.edge,
                "reason": t.reason,
            })
        })
        .collect();
    match view {
        View::Summary => pretty(&json!({
            "run_id": run_id,
            "started_as": history.summary,
            "step": state.seq,
            "last_step": history.last_seq,
            "status": state.status,
            "stage": state.cursor.stage,
            "iteration": state.cursor.iteration,
            "phase": state.phase,
            "totals": state.totals,
            "children": state.children,
            "wait_reason": state.wait_reason,
            "final_output": state.final_output,
            "answer": history.answer.as_ref().map(|answer| match answer {
                Ok(text) => json!(text),
                Err(why) => json!({ "unreadable": why }),
            }),
            "transitions": transitions.len(),
            "last_transition": transitions.last(),
        })),
        View::State => format!(
            "The state of '{run_id}' at step {} (its file records steps up to {}):\n{}",
            state.seq,
            history.last_seq,
            leviath_runtime::runfile::view::state_toml(state)
        ),
        View::Transitions => format!(
            "'{run_id}' took {} edge{}:\n{}",
            transitions.len(),
            if transitions.len() == 1 { "" } else { "s" },
            pretty(&Value::Array(transitions))
        ),
    }
}
