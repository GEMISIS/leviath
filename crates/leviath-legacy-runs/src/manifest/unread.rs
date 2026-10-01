//! The keys of a manifest that nothing reads.
//!
//! Most of a manifest's tables ignore a key the parser does not know, so a
//! misspelled or retired setting loads as if it were not there. Converting
//! such a manifest would drop the setting without a word; `migrate` reports
//! each one instead. The tables that already refuse a stranger (a stage, its
//! context, routing, hooks, edges, gates and sandboxes) are not walked here.

use toml::{Table, Value};

use super::model::MODEL_KEYS;
use super::regions::REGION_KEYS;
use super::sections::{DEPENDENCIES_KEYS, OUTPUT_KEYS};
use super::stage::INTERACTION_POINT_KEYS;

/// The tables a manifest may hold.
const TOP_KEYS: &[&str] = &[
    "agent",
    "compaction",
    "context",
    "dependencies",
    "mcp_servers",
    "mime_types",
    "read_paths",
    "repetition_detection",
    "safe_commands",
    "sandbox",
    "security",
    "stages",
    "tool_permissions",
    "tool_script_permissions",
    "transforms",
];

/// The keys of the top-level `[context]` table.
const CONTEXT_KEYS: &[&str] = &["regions", "file_tracking"];
const FILE_TRACKING_KEYS: &[&str] = &["region", "track_reads", "track_writes", "max_file_tokens"];
const COMPACTION_KEYS: &[&str] = &[
    "provider",
    "model",
    "system_prompt",
    "max_summary_tokens",
    "temperature",
];
const SECURITY_KEYS: &[&str] = &["taint_tracking"];
const READ_PATHS_KEYS: &[&str] = &["allow"];
const SAFE_COMMANDS_KEYS: &[&str] = &["tools", "shell"];
const REPETITION_KEYS: &[&str] = &["max_repeat_calls", "max_readonly_streak", "enabled"];
const NUDGE_KEYS: &[&str] = &["enabled", "max", "text"];
const ARTIFACT_KEYS: &[&str] = &["name", "type", "required", "description"];
const TRANSFORM_KEYS: &[&str] = &["from_blueprint", "to_blueprint", "mappings"];
const MAPPING_KEYS: &[&str] = &["from_region", "to_region", "transform", "fields"];
const INSTALL_KEYS: &[&str] = &["command", "commands", "script", "server"];
const SERVER_KEYS: &[&str] = &["transport", "command", "url", "args", "headers", "env"];

/// Every key of `manifest` that nothing reads, one line each, naming where it
/// is.
pub(crate) fn unread_keys(manifest: &Table) -> Vec<String> {
    let mut found = Unread(Vec::new());
    found.keys("the manifest", Some(manifest), TOP_KEYS);
    let agent = table(manifest.get("agent"));
    found.keys("[agent]", agent, super::AGENT_KEYS);
    found.output("[agent.output]", agent.and_then(|a| table(a.get("output"))));
    found.keys(
        "[agent.nudge]",
        agent.and_then(|a| table(a.get("nudge"))),
        NUDGE_KEYS,
    );
    let context = table(manifest.get("context"));
    found.keys("[context]", context, CONTEXT_KEYS);
    found.regions("", context);
    found.keys(
        "[context.file_tracking]",
        context.and_then(|c| table(c.get("file_tracking"))),
        FILE_TRACKING_KEYS,
    );
    for (at, key, known) in [
        ("[compaction]", "compaction", COMPACTION_KEYS),
        ("[security]", "security", SECURITY_KEYS),
        ("[read_paths]", "read_paths", READ_PATHS_KEYS),
        ("[safe_commands]", "safe_commands", SAFE_COMMANDS_KEYS),
        (
            "[repetition_detection]",
            "repetition_detection",
            REPETITION_KEYS,
        ),
    ] {
        found.keys(at, table(manifest.get(key)), known);
    }
    for (i, transform) in tables(manifest.get("transforms")).enumerate() {
        let at = format!("[[transforms]] {i}");
        found.keys(&at, Some(transform), TRANSFORM_KEYS);
        for (j, mapping) in tables(transform.get("mappings")).enumerate() {
            found.keys(&format!("{at} mapping {j}"), Some(mapping), MAPPING_KEYS);
        }
    }
    for (i, dep) in tables(manifest.get("dependencies")).enumerate() {
        let at = format!("[[dependencies]] {i}");
        found.keys(&at, Some(dep), DEPENDENCIES_KEYS);
        let install = table(dep.get("install"));
        found.keys(&format!("{at} install"), install, INSTALL_KEYS);
        found.keys(
            &format!("{at} install.server"),
            install.and_then(|i| table(i.get("server"))),
            SERVER_KEYS,
        );
    }
    for (name, stage) in table(manifest.get("stages")).into_iter().flatten() {
        let stage = table(Some(stage));
        let sub = |key: &str| stage.and_then(|s| table(s.get(key)));
        let at = |table: &str| format!("[stages.{name}.{table}]");
        found.keys(&at("model"), sub("model"), MODEL_KEYS);
        found.output(&at("output"), sub("output"));
        found.keys(&at("nudge"), sub("nudge"), NUDGE_KEYS);
        found.keys(&at("security"), sub("security"), SECURITY_KEYS);
        found.regions(&format!("stage '{name}' "), sub("context"));
        let points = stage.and_then(|s| s.get("interaction_points"));
        for (i, point) in tables(points).enumerate() {
            found.keys(
                &format!("[stages.{name}] interaction point {i}"),
                Some(point),
                INTERACTION_POINT_KEYS,
            );
        }
    }
    found.0
}

/// The lines found so far.
struct Unread(Vec<String>);

impl Unread {
    /// Every key of `table` (when there is one) that is not in `known`.
    fn keys(&mut self, at: &str, table: Option<&Table>, known: &[&str]) {
        for key in table.into_iter().flat_map(Table::keys) {
            if !known.contains(&key.as_str()) {
                let hint = match (at, key.as_str()) {
                    ("the manifest", "nudge") => " (an agent's nudge settings are [agent.nudge])",
                    _ => "",
                };
                self.0.push(format!(
                    "{at}: `{key}` is not read, so the new file would not have it{hint}"
                ));
            }
        }
    }

    /// An output table and each artifact it lists.
    fn output(&mut self, at: &str, output: Option<&Table>) {
        self.keys(at, output, OUTPUT_KEYS);
        let artifacts = output.and_then(|o| o.get("artifacts"));
        for (i, artifact) in tables(artifacts).enumerate() {
            self.keys(&format!("{at} artifact {i}"), Some(artifact), ARTIFACT_KEYS);
        }
    }

    /// Each region of a `context` table's `regions`, named after `prefix`.
    fn regions(&mut self, prefix: &str, context: Option<&Table>) {
        let regions = context.and_then(|c| table(c.get("regions")));
        for (name, region) in regions.into_iter().flatten() {
            self.keys(
                &format!("{prefix}region '{name}'"),
                table(Some(region)),
                REGION_KEYS,
            );
        }
    }
}

fn table(value: Option<&Value>) -> Option<&Table> {
    value.and_then(Value::as_table)
}

/// The tables of an array of tables; anything else in it is skipped.
fn tables(value: Option<&Value>) -> impl Iterator<Item = &Table> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_table)
}

#[cfg(test)]
#[path = "unread_tests.rs"]
mod tests;
