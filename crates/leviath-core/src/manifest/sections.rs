//! Parsing the standalone top-level tables - the ones that configure the agent
//! as a whole rather than any one stage.

use super::*;

/// Parse `[compaction]` over the defaults, leaving any field the manifest does
/// not mention at its default rather than at zero.
pub(super) fn parse_compaction_config(table: &toml::value::Table) -> CompactionConfig {
    let mut cc = CompactionConfig::default();

    if let Some(provider) = table.get("provider").and_then(|v| v.as_str()) {
        cc.provider = provider.to_string();
    }
    if let Some(model) = table.get("model").and_then(|v| v.as_str()) {
        cc.model = model.to_string();
    }
    if let Some(sp) = table.get("system_prompt").and_then(|v| v.as_str()) {
        cc.system_prompt = Some(sp.to_string());
    }
    if let Some(mst) = table.get("max_summary_tokens").and_then(|v| v.as_integer()) {
        cc.max_summary_tokens = mst as usize;
    }
    if let Some(temp) = table.get("temperature").and_then(|v| v.as_float()) {
        cc.temperature = temp as f32;
    }

    cc
}

/// Parse `[read_paths]`. Entries are syntax-checked here so a broken one fails
/// `lev validate`/`lev add`/spawn loudly, instead of degrading the agent at its
/// first out-of-workdir read.
pub(super) fn parse_read_paths(
    table: &toml::value::Table,
) -> Result<crate::blueprint::ReadPathsConfig> {
    let mut allow = Vec::new();
    if let Some(entries) = table.get("allow").and_then(|v| v.as_array()) {
        for entry in entries {
            let Some(raw) = entry.as_str() else {
                return Err(Error::Other(format!(
                    "[read_paths] allow entries must be strings, got: {entry}"
                )));
            };
            crate::read_paths::validate_entry_syntax(raw).map_err(Error::Other)?;
            allow.push(raw.to_string());
        }
    }
    Ok(crate::blueprint::ReadPathsConfig { allow })
}

/// Parse `[safe_commands]`: what this agent would like to run unprompted.
///
/// Inert until the user opts in, so parsing is permissive - but a non-string
/// entry is still a hard error, because a list that silently loses members
/// reads as a grant that was made.
pub(super) fn parse_safe_commands(
    table: &toml::value::Table,
) -> Result<crate::blueprint::SafeCommandsConfig> {
    let strings = |field: &str| -> Result<Vec<String>> {
        let Some(entries) = table.get(field).and_then(|v| v.as_array()) else {
            return Ok(Vec::new());
        };
        entries
            .iter()
            .map(|entry| {
                entry.as_str().map(str::to_string).ok_or_else(|| {
                    Error::Other(format!(
                        "[safe_commands] {field} entries must be strings, got: {entry}"
                    ))
                })
            })
            .collect()
    };
    Ok(crate::blueprint::SafeCommandsConfig {
        tools: strings("tools")?,
        shell: strings("shell")?,
    })
}

/// Flatten `[tool_permissions]` into the `tool_perm:<tool>` metadata keys the
/// permission layer reads. A non-string policy is dropped rather than rejected:
/// the value's meaning is validated where it is resolved.
pub(super) fn tool_permission_metadata(
    table: &toml::value::Table,
) -> impl Iterator<Item = (String, serde_json::Value)> + use<'_> {
    table.iter().filter_map(|(tool_name, policy_val)| {
        policy_val.as_str().map(|policy| {
            (
                format!("tool_perm:{}", tool_name),
                serde_json::Value::String(policy.to_string()),
            )
        })
    })
}

/// Parse `[context.file_tracking]`. Tracking both directions into a `files`
/// region is the default because that is what the shipped layouts assume.
pub(super) fn parse_file_tracking(table: &toml::value::Table) -> crate::FileTrackingConfig {
    crate::FileTrackingConfig {
        region: table
            .get("region")
            .and_then(|v| v.as_str())
            .unwrap_or("files")
            .to_string(),
        track_reads: table
            .get("track_reads")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        track_writes: table
            .get("track_writes")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        max_file_tokens: table
            .get("max_file_tokens")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize),
    }
}

/// Parse `[repetition_detection]`. Every field stays `None` when absent so the
/// global config's value survives; there are no local defaults to apply here.
pub(super) fn parse_repetition_detection(
    table: &toml::value::Table,
) -> crate::RepetitionDetectionConfig {
    crate::RepetitionDetectionConfig {
        max_repeat_calls: table
            .get("max_repeat_calls")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize),
        max_readonly_streak: table
            .get("max_readonly_streak")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize),
        enabled: table.get("enabled").and_then(|v| v.as_bool()),
    }
}

/// Parse an `[agent.output]` or `[stages.<name>.output]` block.
///
/// `format` is read as an opaque string and never matched against a known set:
/// a value this parser has never seen is as valid as `"markdown"`, which is what
/// lets a blueprint ask for a2ui, a house schema, or a format invented after
/// this code was written without touching it.
///
/// `schema` is taken as arbitrary TOML and converted to JSON, so an author can
/// write the schema inline as a TOML table rather than embedding a JSON string.
pub(super) fn parse_output_spec(table: &toml::value::Table) -> crate::output::OutputSpec {
    let string_field = |key: &str| {
        table
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
    };
    crate::output::OutputSpec {
        format: string_field("format"),
        instructions: string_field("instructions"),
        example: string_field("example"),
        // A schema that will not convert is dropped rather than fatal: the
        // validator itself already treats an uncompilable schema as "skip the
        // check" rather than "refuse every submission", and disagreeing here
        // would make the same bad schema fatal at load and harmless at dispatch.
        schema: table
            .get("schema")
            .and_then(|v| serde_json::to_value(v).ok()),
        validator: string_field("validator"),
    }
}

pub(super) fn parse_security_config(security_table: &toml::value::Table) -> crate::SecurityConfig {
    let mut sc = crate::SecurityConfig::default();
    if let Some(tt) = security_table
        .get("taint_tracking")
        .and_then(|v| v.as_bool())
    {
        sc.taint_tracking = tt;
    }
    sc
}

/// Parse a `[sandbox]` / `[stages.X.sandbox]` table into a `ToolSandboxConfig`.
/// A present block with no `kind` means host passthrough; omit the block to
/// inherit the broader (agent/global) sandbox. An unknown `kind` or
/// `on_unavailable` value is a hard error rather than a silently-ignored
/// misconfiguration (mirrors transition-condition/transform validation).
pub(super) fn parse_sandbox_config(
    table: &toml::value::Table,
) -> Result<crate::sandbox::ToolSandboxConfig> {
    use crate::sandbox::{OnUnavailable, SandboxKind, ToolSandboxConfig};

    let mut sc = ToolSandboxConfig::default();

    if let Some(kind) = table.get("kind").and_then(|v| v.as_str()) {
        sc.kind = match kind {
            "none" => SandboxKind::None,
            "namespace" => SandboxKind::Namespace,
            "container" => SandboxKind::Container,
            other => {
                return Err(Error::Other(format!(
                    "sandbox has unknown kind '{other}' \
                     (valid: none, namespace, container)"
                )));
            }
        };
    }
    if let Some(image) = table.get("image").and_then(|v| v.as_str()) {
        sc.image = Some(image.to_string());
    }
    if let Some(engine) = table.get("engine").and_then(|v| v.as_str()) {
        sc.engine = Some(engine.to_string());
    }
    if let Some(network) = table.get("network").and_then(|v| v.as_bool()) {
        sc.network = network;
    }
    if let Some(persist) = table.get("persist").and_then(|v| v.as_bool()) {
        sc.persist = persist;
    }
    if let Some(mounts) = table.get("mount").and_then(|v| v.as_array()) {
        sc.mounts = mounts
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect();
    }
    if let Some(ou) = table.get("on_unavailable").and_then(|v| v.as_str()) {
        sc.on_unavailable = match ou {
            "error" => OnUnavailable::Error,
            "warn" => OnUnavailable::Warn,
            other => {
                return Err(Error::Other(format!(
                    "sandbox has unknown on_unavailable '{other}' \
                     (valid: error, warn)"
                )));
            }
        };
    }
    Ok(sc)
}
